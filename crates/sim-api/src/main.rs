//! GivEnergy Simulator — headless CLI.
//!
//! `giv-sim run scenario.yaml`

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use sim_core::{
    BatteryEngine, Command, InverterEngine, LoadEngine, LoadProfile, PlantState,
    SimulationEngine, SolarEngine, WeatherCondition,
};
use sim_models::DeviceModel;
use sim_scenarios::parse_scenario;
use std::net::SocketAddr;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "giv-sim", version, about = "GivEnergy hardware simulator")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a scenario YAML file.
    Run {
        /// Path to the scenario file.
        #[arg(value_name = "SCENARIO")]
        scenario: std::path::PathBuf,
        /// Tick interval in seconds.
        #[arg(long, default_value = "30")]
        tick_interval: u64,
        /// Start date (YYYY-MM-DD).
        #[arg(long, default_value = "2025-06-01")]
        date: String,
        /// Solar peak capacity in watts.
        #[arg(long, default_value = "5000")]
        peak_watts: f64,
        /// Site latitude (degrees, positive = north).
        #[arg(long, default_value = "51.5")]
        latitude: f64,
        /// Load profile: minimal, family, ev, heatpump.
        #[arg(long, default_value = "family")]
        profile: String,
        /// Weather: clear, partly-cloudy, overcast, storm.
        #[arg(long, default_value = "clear")]
        weather: String,
        /// Also launch a Modbus TCP server on this address.
        #[arg(long)]
        modbus: Option<SocketAddr>,
    },
}

fn parse_weather(s: &str) -> WeatherCondition {
    match s.to_lowercase().as_str() {
        "partly-cloudy" | "partly_cloudy" | "partlycloudy" => WeatherCondition::PartlyCloudy,
        "overcast" => WeatherCondition::Overcast,
        "storm" => WeatherCondition::Storm,
        _ => WeatherCondition::Clear,
    }
}

fn parse_profile(s: &str) -> LoadProfile {
    match s.to_lowercase().as_str() {
        "minimal" => LoadProfile::Minimal,
        "ev" => LoadProfile::EV,
        "heatpump" | "heat-pump" | "heat_pump" => LoadProfile::HeatPump,
        _ => LoadProfile::Family,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giv_sim=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            scenario,
            tick_interval,
            date,
            peak_watts,
            latitude,
            profile,
            weather,
            modbus,
        } => {
            let yaml = std::fs::read_to_string(&scenario)?;
            let scen = parse_scenario(&yaml)?;

            let start_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")?;
            let start_ts = start_date.and_hms_opt(0, 0, 0).unwrap();

            let state = PlantState::new(start_ts);

            let mut solar = SolarEngine::new(peak_watts, latitude);
            solar.weather = parse_weather(&weather);

            let load_profile = parse_profile(&profile);

            let devices: Vec<Box<dyn DeviceModel>> = vec![
                Box::new(solar),
                Box::new(LoadEngine::new(load_profile)),
                Box::new(InverterEngine::new()),
                Box::new(BatteryEngine::new()),
            ];

            let mut engine = SimulationEngine::new(state, devices, tick_interval);

            tracing::info!(
                "Running scenario '{}' ({} events, tick={}s, profile={}, weather={:?})",
                scen.name,
                scen.events.len(),
                tick_interval,
                profile,
                parse_weather(&weather),
            );

            // Optional: launch Modbus server in background
            if let Some(addr) = modbus {
                let reg_cat = sim_registers::default_register_catalogue();
                let store = sim_registers::RegisterStore::new(reg_cat);
                let store = std::sync::Arc::new(tokio::sync::Mutex::new(store));
                tokio::spawn(async move {
                    if let Err(e) = sim_modbus::run_modbus_server(addr, store).await {
                        tracing::error!("Modbus server error: {e}");
                    }
                });
                tracing::info!("Modbus TCP server starting on {addr}");
            }

            // Run ticks, applying scenario events at matching times
            for (time, event) in &scen.events {
                let target = start_date.and_time(*time);
                while engine.state.timestamp < target {
                    engine.tick();
                }

                // Apply event overrides
                if let Some(solar) = event.solar {
                    engine.state.solar.generation_w = solar;
                }
                if let Some(load) = event.load {
                    engine.state.load.demand_w = load;
                }
                if let Some(fault) = &event.fault {
                    engine.enqueue(Command::InjectFault(fault.clone()));
                }
                if let Some(fault) = &event.clear_fault {
                    engine.enqueue(Command::ClearFault(fault.clone()));
                }

                // Tick once to let the event take effect
                engine.tick();

                // Check assertions
                if let Some(expect) = &event.expect {
                    match sim_scenarios::check_assertions(expect, &engine.state) {
                        Ok(()) => {
                            tracing::info!(
                                "[{}] ✓ assertions passed (SOC={:.1}%)",
                                time,
                                engine.state.battery.soc_percent,
                            );
                        }
                        Err(failures) => {
                            tracing::error!(
                                "[{}] ✗ assertion failures: {:?}",
                                time,
                                failures,
                            );
                        }
                    }
                }
            }

            tracing::info!("Scenario complete.");
            tracing::info!(
                "Final state: SOC={:.1}%, solar={:.0}W, load={:.0}W, grid={:.0}W",
                engine.state.battery.soc_percent,
                engine.state.solar.generation_w,
                engine.state.load.demand_w,
                engine.state.grid.power_w,
            );
        }
    }

    Ok(())
}
