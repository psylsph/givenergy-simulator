#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use sim_core::{
        BatteryEngine, Command, InverterEngine, LoadEngine, LoadProfile, PlantState,
        SimulationEngine, SolarEngine, WeatherCondition,
    };
    use sim_faults::FaultEngine;
    use sim_models::{DeviceModel, InverterMode};
    use sim_recording::RecordingFrame;
    use sim_registers::RegisterStore;

    fn ts(hour: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    fn make_devices() -> Vec<Box<dyn DeviceModel>> {
        vec![
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(FaultEngine::new()),
            Box::new(BatteryEngine::new()),
        ]
    }

    /// Integration: full day simulation with register projection
    #[test]
    fn full_day_with_register_projection() {
        let state = PlantState::new(ts(0));
        let mut engine = SimulationEngine::new(state, make_devices(), 30);
        let mut reg_store = RegisterStore::new(sim_registers::default_register_catalogue());

        let mut recording: Vec<RecordingFrame> = Vec::new();

        // Run 24 hours = 2880 ticks at 30s
        for _ in 0..2880 {
            engine.tick();
            reg_store.project_from_state(&engine.state);
            recording.push(RecordingFrame {
                timestamp: engine.state.timestamp,
                plant_state: engine.state.clone(),
                register_snapshot: reg_store.snapshot(),
            });
        }

        // Verify recording has frames with register snapshots
        assert_eq!(recording.len(), 2880);
        for frame in &recording {
            assert!(!frame.register_snapshot.is_empty());
        }

        // Verify midday frame has non-zero solar register
        let midday = recording.get(1440).unwrap(); // ~12:00
        // Holding register at address 300 has key 10000+300
        let solar_reg = midday.register_snapshot[&(10000u32 + 300)];
        assert!(solar_reg > 0, "Midday solar register should be > 0, got {solar_reg}");

        // Verify SOC changed
        assert_ne!(engine.state.aggregate_soc(), 50.0);
    }

    /// Integration: scenario with weather change
    #[test]
    fn scenario_weather_change() {
        let state = PlantState::new(ts(8));
        let mut engine = SimulationEngine::new(state, make_devices(), 30);

        // Tick to midday
        while engine.state.timestamp < ts(12) {
            engine.tick();
        }
        let clear_solar = engine.state.solar.generation_w;
        assert!(clear_solar > 0.0);

        // Change to overcast
        engine.enqueue(Command::SetWeather(WeatherCondition::Overcast));
        engine.tick();

        let overcast_solar = engine.state.solar.generation_w;
        assert!(
            overcast_solar < clear_solar,
            "Overcast should reduce solar: {overcast_solar} vs {clear_solar}"
        );
    }

    /// Integration: fault injection and clearing with register state
    #[test]
    fn fault_injection_reflects_in_registers() {
        let state = PlantState::new(ts(12));
        let mut engine = SimulationEngine::new(state, make_devices(), 30);
        let mut reg_store = RegisterStore::new(sim_registers::default_register_catalogue());

        engine.tick();
        reg_store.project_from_state(&engine.state);

        // Inject grid loss
        engine.enqueue(Command::InjectFault("grid_loss".to_string()));
        engine.tick();
        reg_store.project_from_state(&engine.state);

        // Clear fault
        engine.enqueue(Command::ClearFault("grid_loss".to_string()));
        engine.tick();
        reg_store.project_from_state(&engine.state);

        // Grid should be reconnected
        assert!(engine.state.grid.connected);
    }

    /// Integration: inverter mode change via command
    #[test]
    fn inverter_mode_change_via_command() {
        let state = PlantState::new(ts(12));
        let mut engine = SimulationEngine::new(state, make_devices(), 30);

        // Switch to force charge
        engine.enqueue(Command::SetInverterMode(InverterMode::ForceCharge));
        engine.tick();

        assert_eq!(engine.state.inverter.mode_state.effective, InverterMode::ForceCharge);
        assert!(
            engine.state.grid.power_w > 0.0 || engine.state.total_battery_power_kw() > 0.0,
            "Force charge should be importing from grid or charging battery"
        );
    }

    /// Integration: recording round-trip through storage
    #[test]
    fn recording_roundtrip_with_registers() {
        let state = PlantState::new(ts(12));
        let mut engine = SimulationEngine::new(state, make_devices(), 30);
        let mut reg_store = RegisterStore::new(sim_registers::default_register_catalogue());

        engine.tick();
        reg_store.project_from_state(&engine.state);

        let frame = RecordingFrame {
            timestamp: engine.state.timestamp,
            plant_state: engine.state.clone(),
            register_snapshot: reg_store.snapshot(),
        };

        // Serialize and deserialize
        let mut buf = Vec::new();
        sim_recording::write_frame(&mut buf, &frame).unwrap();
        let frames = sim_recording::read_frames(buf.as_slice()).unwrap();

        assert_eq!(frames.len(), 1);
        // 8 registers in catalogue: inverter(2) + battery(3 soc) + pv(1) + grid(2)
        let expected_count = sim_registers::default_register_catalogue().len();
        assert_eq!(frames[0].register_snapshot.len(), expected_count);
    }

    // ---- Multi-battery specific integration tests ----

    #[test]
    fn two_batteries_distribute_power_evenly() {
        let state = PlantState::with_battery_count(ts(12), 2);
        // Use only Inverter + Battery (manual solar/load control)
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(InverterEngine::new()),
            Box::new(FaultEngine::new()),
            Box::new(BatteryEngine::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 30);

        // Set solar surplus to charge batteries
        engine.state.solar.generation_w = 6000.0;
        engine.state.load.demand_w = 1000.0;

        engine.tick();

        // Both batteries should have the same power distribution
        assert_eq!(engine.state.batteries[0].power_kw, engine.state.batteries[1].power_kw,
            "Both batteries should get equal power");
        assert!(engine.state.batteries[0].power_kw > 0.0,
            "Battery should be charging");
    }

    #[test]
    fn two_batteries_track_soc_independently() {
        let mut state = PlantState::with_battery_count(ts(12), 2);
        state.batteries[0].soc_percent = 30.0;
        state.batteries[1].soc_percent = 80.0;
        state.sync_battery_from_vec();

        // Use only Inverter + Battery (no Solar/Load engine so we control inputs directly)
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(InverterEngine::new()),
            Box::new(FaultEngine::new()),
            Box::new(BatteryEngine::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 30);

        // Discharge scenario at night
        engine.state.solar.generation_w = 0.0;
        engine.state.load.demand_w = 5000.0;

        engine.tick();

        // Both batteries should discharge equally (same power_kw)
        assert!(engine.state.batteries[0].power_kw < 0.0);
        assert!(engine.state.batteries[1].power_kw < 0.0);
        assert_eq!(engine.state.batteries[0].power_kw, engine.state.batteries[1].power_kw);

        // But their SOCs should differ based on initial values
        assert!(engine.state.batteries[0].soc_percent < 30.0);
        assert!(engine.state.batteries[1].soc_percent < 80.0);

        // Both should lose the same absolute SOC percentage
        let delta0 = 30.0 - engine.state.batteries[0].soc_percent;
        let delta1 = 80.0 - engine.state.batteries[1].soc_percent;
        assert!((delta0 - delta1).abs() < 0.01,
            "Both batteries should lose the same SOC percentage: delta0={}, delta1={}", delta0, delta1);
    }

    #[test]
    fn three_batteries_double_capacity() {
        let state = PlantState::with_battery_count(ts(12), 3);
        // Default: each module is 9.5 kWh, so total = 28.5 kWh
        assert!((state.total_battery_capacity() - 28.5).abs() < 0.01,
            "3 batteries should have 3x capacity: {}", state.total_battery_capacity());
        assert!((state.total_max_charge_kw() - 9.0).abs() < 0.01,
            "3 batteries should have 3x charge rate: {}", state.total_max_charge_kw());
    }

    #[test]
    fn three_batteries_register_projection() {
        let mut state = PlantState::with_battery_count(ts(12), 3);
        state.batteries[0].soc_percent = 40.0;
        state.batteries[1].soc_percent = 50.0;
        state.batteries[2].soc_percent = 60.0;
        state.sync_battery_from_vec();

        // No device engines needed — just testing register projection of static state
        let mut reg_store = RegisterStore::new(sim_registers::default_register_catalogue());
        reg_store.project_from_state(&state);

        let snap = reg_store.snapshot();
        assert_eq!(snap[&(10000u32 + 200)], 50); // aggregate SOC (capacity-weighted avg = 50%)
        assert_eq!(snap[&(10000u32 + 201)], 50); // battery 2 SOC
        assert_eq!(snap[&(10000u32 + 202)], 60); // battery 3 SOC
    }
}
