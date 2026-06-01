//! GivEnergy Simulator — headless CLI.
//!
//! `giv-sim run scenario.yaml`
//!
//! Outputs: JSON report, JUnit XML, CSV traces, JSONL recording.

use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use sim_core::{
    BatteryEngine, Command, InverterEngine, LoadEngine, LoadProfile, PlantState,
    SimulationEngine, SolarEngine, WeatherCondition,
};
use sim_faults::FaultEngine;
use sim_models::DeviceModel;
use sim_recording::{RecordingFrame, write_csv, write_frame, write_json_report, write_junit_xml};
use sim_scenarios::{AssertionResult, ScenarioResult, parse_named_scenario};
use std::net::SocketAddr;
use std::path::PathBuf;

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
        scenario: PathBuf,
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
        /// Output directory for reports and traces.
        #[arg(short, long)]
        output: Option<PathBuf>,
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

fn parse_mode_cmd(s: &str) -> Option<sim_models::InverterMode> {
    use sim_models::InverterMode::*;
    match s {
        "Normal" => Some(Normal),
        "Eco" => Some(Eco),
        "ForceCharge" => Some(ForceCharge),
        "ForceDischarge" => Some(ForceDischarge),
        "ExportLimit" => Some(ExportLimit),
        _ => None,
    }
}

fn _parse_weather_cmd(s: &str) -> Option<WeatherCondition> {
    match s {
        "Clear" => Some(WeatherCondition::Clear),
        "PartlyCloudy" => Some(WeatherCondition::PartlyCloudy),
        "Overcast" => Some(WeatherCondition::Overcast),
        "Storm" => Some(WeatherCondition::Storm),
        _ => None,
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
            output,
            modbus,
        } => {
            run_scenario(
                &scenario,
                tick_interval,
                &date,
                peak_watts,
                latitude,
                &profile,
                &weather,
                output.as_deref(),
                modbus,
            )
            .await
        }
    }
}

async fn run_scenario(
    scenario_path: &PathBuf,
    tick_interval: u64,
    date: &str,
    peak_watts: f64,
    latitude: f64,
    profile: &str,
    weather: &str,
    output_dir: Option<&std::path::Path>,
    modbus: Option<SocketAddr>,
) -> Result<(), Box<dyn std::error::Error>> {
    let yaml = std::fs::read_to_string(scenario_path)?;
    let scen = parse_named_scenario(&yaml)?;

    let start_date = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let start_ts = start_date.and_hms_opt(0, 0, 0).unwrap();

    let state = PlantState::new(start_ts);

    let mut solar = SolarEngine::new(peak_watts, latitude);
    solar.weather = parse_weather(weather);

    let load_profile = parse_profile(profile);

    // Order: Solar → Load → Inverter → Faults → Battery
    let devices: Vec<Box<dyn DeviceModel>> = vec![
        Box::new(solar),
        Box::new(LoadEngine::new(load_profile)),
        Box::new(InverterEngine::new()),
        Box::new(FaultEngine::new()),
        Box::new(BatteryEngine::new()),
    ];

    let mut engine = SimulationEngine::new(state, devices, tick_interval);

    // Create output directory if needed
    if let Some(dir) = output_dir {
        std::fs::create_dir_all(dir)?;
    }

    // Recording buffer
    let mut recording: Vec<RecordingFrame> = Vec::new();
    let mut scenario_result = ScenarioResult {
        name: scen.name.clone(),
        passed: 0,
        failed: 0,
        assertions: Vec::new(),
    };

    tracing::info!(
        "Running scenario '{}' ({} events, tick={}s, profile={}, weather={:?})",
        scen.name,
        scen.events.len(),
        tick_interval,
        profile,
        parse_weather(weather),
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

            // Record frame
            recording.push(RecordingFrame {
                timestamp: engine.state.timestamp,
                plant_state: engine.state.clone(),
                register_snapshot: std::collections::HashMap::new(),
            });
        }

        // Apply event overrides
        if let Some(solar_w) = event.solar {
            engine.state.solar.generation_w = solar_w;
        }
        if let Some(load_w) = event.load {
            engine.state.load.demand_w = load_w;
        }
        if let Some(fault) = &event.fault {
            engine.enqueue(Command::InjectFault(fault.clone()));
        }
        if let Some(fault) = &event.clear_fault {
            engine.enqueue(Command::ClearFault(fault.clone()));
        }
        if let Some(mode_str) = &event.mode {
            if let Some(mode) = parse_mode_cmd(mode_str) {
                engine.enqueue(Command::SetInverterMode(mode));
            }
        }
        if let Some(limit) = event.export_limit {
            engine.enqueue(Command::SetExportLimit(limit));
        }

        // Tick once to let the event take effect
        engine.tick();
        recording.push(RecordingFrame {
            timestamp: engine.state.timestamp,
            plant_state: engine.state.clone(),
            register_snapshot: std::collections::HashMap::new(),
        });

        // Check assertions
        if let Some(expect) = &event.expect {
            let time_str = format!("{}", time);
            match sim_scenarios::check_assertions(expect, &engine.state) {
                Ok(()) => {
                    tracing::info!(
                        "[{}] ✓ assertions passed (SOC={:.1}%)",
                        time,
                        engine.state.battery.soc_percent,
                    );
                    scenario_result.passed += 1;
                    scenario_result.assertions.push(AssertionResult {
                        time: time_str,
                        passed: true,
                        messages: vec![],
                    });
                }
                Err(failures) => {
                    tracing::error!(
                        "[{}] ✗ assertion failures: {:?}",
                        time,
                        failures,
                    );
                    scenario_result.failed += 1;
                    scenario_result.assertions.push(AssertionResult {
                        time: time_str,
                        passed: false,
                        messages: failures,
                    });
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

    // Write outputs
    if let Some(dir) = output_dir {
        let base_name = scenario_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");

        // JSONL recording
        let jsonl_path = dir.join(format!("{base_name}.jsonl"));
        let mut f = std::fs::File::create(&jsonl_path)?;
        for frame in &recording {
            write_frame(&mut f, frame)?;
        }
        tracing::info!("Recording: {} ({} frames)", jsonl_path.display(), recording.len());

        // CSV traces
        let csv_path = dir.join(format!("{base_name}.csv"));
        let mut f = std::fs::File::create(&csv_path)?;
        write_csv(&mut f, &recording)?;
        tracing::info!("CSV traces: {}", csv_path.display());

        // JUnit XML
        let junit_path = dir.join(format!("{base_name}.xml"));
        let mut f = std::fs::File::create(&junit_path)?;
        write_junit_xml(&mut f, &scenario_result)?;
        tracing::info!("JUnit XML: {}", junit_path.display());

        // JSON report
        let report_path = dir.join(format!("{base_name}_report.json"));
        let mut f = std::fs::File::create(&report_path)?;
        write_json_report(&mut f, &scenario_result)?;
        tracing::info!("JSON report: {}", report_path.display());
    }

    if scenario_result.failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}
