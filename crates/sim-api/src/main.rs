//! GivEnergy Plant Simulator — headless CLI.
//!
//! `giv-sim simulate --inverter Gen3Hybrid --batteries 2 --battery-size 9.5`
//! `giv-sim run scenario.yaml`
//! `giv-sim serve plant_state.json --modbus 0.0.0.0:8899`
//!
//! Outputs: JSON report, JUnit XML, CSV traces, JSONL recording.

#![allow(clippy::too_many_arguments, clippy::collapsible_if, clippy::ptr_arg)]

use chrono::NaiveDate;
use clap::Parser;
use sim_core::{
    BatteryEngine, Command, InverterEngine, LoadEngine, LoadProfile, PlantState, SimulationEngine,
    SolarEngine, WeatherCondition,
};
use sim_faults::FaultEngine;
use sim_models::DeviceModel;
use sim_recording::{RecordingFrame, write_csv, write_frame, write_json_report, write_junit_xml};
use sim_scenarios::{AssertionResult, ScenarioResult, parse_named_scenario};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Wrapper for import/export of plant configuration.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlantConfig {
    plant: sim_models::PlantState,
    schedule: Option<sim_core::Schedule>,
}

#[derive(clap::Parser)]
#[command(
    name = "giv-sim",
    version,
    about = "GivEnergy Plant Simulator",
    long_about = "GivEnergy Plant Simulator — simulates a GivEnergy solar battery system.\
\n\
Run a headless simulation with the 'simulate' subcommand (starts Modbus server automatically).\
\nRun a scenario YAML with 'run'. Load a saved plant config with 'serve'.\
\n\
\nINVERTER TYPES (use with --inverter):\
\n  Gen1Hybrid          DTC 0x2001  5kW AC, 2.5kW battery\
\n  Gen2Hybrid          DTC 0x2001  5kW AC, 3.6kW battery\
\n  Gen3Hybrid          DTC 0x2001  5kW AC, 3.6kW battery (default)\
\n  Gen3Hybrid8kW       DTC 0x2106  8kW AC, 8kW battery\
\n  Gen3Hybrid10kW      DTC 0x2102  10kW AC, 10kW battery\
\n  Gen3Plus6kW         DTC 0x2201  5kW AC, 2.6kW battery\
\n  Gen3Plus4600        DTC 0x2202  4.6kW AC, 2.6kW battery\
\n  Gen3Plus3600        DTC 0x2203  3.6kW AC, 2.6kW battery\
\n  Gen3Plus6kW2        DTC 0x2204  6kW AC, 2.6kW battery\
\n  ACCoupled           DTC 0x3001  3kW AC, 3kW battery\
\n  ACCoupled2          DTC 0x3002  3kW AC, 3kW battery\
\n  ThreePhase          DTC 0x4001  6kW AC, 6kW battery\
\n  ThreePhase8kW       DTC 0x4002  8kW AC, 8kW battery\
\n  ThreePhase10kW      DTC 0x4003  10kW AC, 10kW battery\
\n  ThreePhase11kW      DTC 0x4004  11kW AC, 11kW battery\
\n  AllInOne6           DTC 0x8001  6kW AC, 6kW battery\
\n  AllInOne            DTC 0x8002  6kW AC, 6kW battery\
\n  AllInOne5           DTC 0x8003  5kW AC, 5kW battery\
\n  AIO8kW              DTC 0x8102  8kW AC, 8kW battery\
\n  AIO10kW             DTC 0x8103  10kW AC, 10kW battery\
\n  AIOHybrid6kW        DTC 0x8201  6kW AC, 6kW battery\
\n  AIOHybrid8kW        DTC 0x8202  8kW AC, 8kW battery\
\n  AIOHybrid10kW       DTC 0x8203  10kW AC, 10kW battery\
\n\
\nBATTERY SIZES (kWh, use with --battery-size):\
\n  2.6, 3.4, 5.2, 6.8, 7.0, 8.2, 9.5, 10.2, 12.8, 13.6, 16.0, 17.0, 19.0, 20.4\
\n\
\nBATTERY COUNT: 1–6 modules (use with --batteries)\
\n\
\nLOAD PROFILES (use with --load-profile):\
\n  minimal    ~200W baseload\
\n  family     Typical family usage pattern (default)\
\n  ev         Family + EV charger pattern\
\n  heatpump   Family + heat pump pattern\
\n  <path>     Custom YAML file with {hour, watts} entries\
\n\
\nWEATHER (use with --weather):\
\n  clear           Full sun\
\n  partly-cloudy   Some cloud cover\
\n  overcast        Heavy cloud cover\
\n  storm           Very low solar generation\
\n\
\nEXAMPLES:\
\n  giv-sim simulate --inverter Gen3Hybrid --batteries 2 --battery-size 9.5\
\n  giv-sim simulate --inverter ThreePhase --batteries 3 --battery-size 13.6 \
\n                   --soc 80 --solar-peak 8000 --load-level 1500\
\n  giv-sim simulate --inverter AllInOne --modbus 0.0.0.0:8899\
\n  giv-sim simulate --inverter Gen3Hybrid --inverter-temperature 70\
\n  # --inverter-temperature pins the inverter temp (°C); omit for thermal model\
\n  giv-sim run scenario.yaml --modbus 0.0.0.0:8899 --output results/\
\n  giv-sim serve plant_state.json --modbus 0.0.0.0:8899\
\n  giv-sim replay recording.jsonl"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Start a headless simulation with auto-configured plant.
    ///
    /// Creates a plant from CLI parameters and starts ticking immediately.
    /// The Modbus TCP server starts automatically on 0.0.0.0:8899 (override with --modbus).
    /// Press Ctrl+C to stop and save the recording.
    Simulate {
        /// Inverter type name (see list above).
        ///
        /// Determines AC power limit, battery power limit, DTC code, and firmware versions.
        #[arg(long, default_value = "Gen3Hybrid")]
        inverter: String,

        /// Number of battery modules (1–6).
        ///
        /// Each module gets the capacity specified by --battery-size.
        /// Total capacity = modules × battery-size.
        #[arg(long, default_value = "1")]
        batteries: usize,

        /// Battery module capacity in kWh.
        ///
        /// Supported sizes: 2.6, 3.4, 5.2, 6.8, 7.0, 8.2, 9.5, 10.2,
        /// 12.8, 13.6, 16.0, 17.0, 19.0, 20.4
        /// Default: 9.5 kWh (GIV-BAT-9.5)
        #[arg(long, default_value = "9.5")]
        battery_size: f64,

        /// Battery state of charge as a percentage (0–100).
        ///
        /// Sets the initial SOC for all battery modules.
        #[arg(long, default_value = "50")]
        soc: f64,

        /// Solar PV peak capacity in watts.
        ///
        /// Determines maximum generation under ideal conditions.
        /// The solar engine generates power based on time-of-day, latitude, and weather.
        #[arg(long, default_value = "5000")]
        solar_peak: f64,

        /// House load level in watts (sets a fixed load override).
        ///
        /// When set, the load engine uses this constant demand instead of
        /// the time-varying profile. Set to 0 to use the load profile instead.
        #[arg(long, default_value = "0")]
        load_level: f64,

        /// Load profile for time-varying demand: minimal, family, ev, heatpump, or path to YAML.
        ///
        /// Ignored if --load-level is non-zero.
        #[arg(long, default_value = "family")]
        load_profile: String,

        /// Weather condition: clear, partly-cloudy, overcast, storm.
        #[arg(long, default_value = "clear")]
        weather: String,

        /// Site latitude in degrees (positive = north).
        ///
        /// Affects solar generation curve — higher latitudes have shorter winter days.
        #[arg(long, default_value = "51.5")]
        latitude: f64,

        /// Start date (YYYY-MM-DD).
        ///
        /// Season affects solar generation (summer = longer days, winter = shorter).
        #[arg(long, default_value = "2025-06-21")]
        date: String,

        /// Simulation tick interval in seconds.
        ///
        /// Each tick advances the simulation clock by this amount.
        /// Smaller values = finer resolution but more CPU.
        #[arg(long, default_value = "1")]
        tick_interval: u64,

        /// Modbus TCP server bind address.
        ///
        /// The GivEnergy proprietary Modbus protocol runs on this port.
        /// Default: 0.0.0.0:8899
        #[arg(long, default_value = "0.0.0.0:8899")]
        modbus: SocketAddr,

        /// Output directory for recording (JSONL + CSV). Created if it doesn't exist.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// PV array 2 peak capacity in watts (0 = single array).
        ///
        /// When > 0, solar generation splits 45% PV1 / 55% PV2.
        #[arg(long, default_value = "0")]
        pv2_peak: f64,

        /// Run in fast-forward (fixed-step) mode instead of real-time.
        ///
        /// By default the simulation clock is locked to the host wall clock so
        /// the served inverter time matches the computer's time exactly (no
        /// drift). Pass --fast to instead advance the clock by `tick_interval`
        /// seconds as fast as the loop allows — useful for burning through a
        /// full day quickly when you don't need wall-clock alignment.
        #[arg(long, default_value_t = false)]
        fast: bool,
        /// Simulate a faulty dongle (Off, EmptyData, StaleData, GarbageData,
        /// DropConnection, Intermittent).
        #[arg(long, default_value = "Off")]
        dongle_misbehaviour: String,
        /// Pin the inverter temperature (°C), bypassing the thermal model.
        ///
        /// Useful for holding a fixed temperature to exercise derating /
        /// over-temperature behaviour (e.g. 70 to approach the over-temp
        /// threshold). Omit to let the thermal model vary it with load.
        #[arg(long)]
        inverter_temperature: Option<f64>,
    },
    /// Run a scenario YAML file.
    Run {
        /// Path to the scenario file.
        #[arg(value_name = "SCENARIO")]
        scenario: PathBuf,
        /// Tick interval in seconds.
        #[arg(long, default_value = "1")]
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
        /// Number of battery modules (1–6).
        #[arg(long, default_value = "1")]
        battery_count: usize,
        /// Also launch a Modbus TCP server on this address.
        #[arg(long)]
        modbus: Option<SocketAddr>,
        /// Simulate a faulty dongle (Off, EmptyData, StaleData, GarbageData,
        /// DropConnection, Intermittent).
        #[arg(long, default_value = "Off")]
        dongle_misbehaviour: String,
    },
    /// Replay a recording or diff two recordings.
    Replay {
        /// Path to recording file (JSON Lines format).
        recording: PathBuf,
        /// Optional second recording for diff.
        #[arg(long)]
        diff: Option<PathBuf>,
        /// Output format: summary, csv, json.
        #[arg(long, default_value = "summary")]
        format: String,
    },
    /// Load a plant config JSON and run headless with Modbus server.
    Serve {
        /// Path to plant config JSON file (exported from GUI).
        #[arg(value_name = "CONFIG")]
        config: PathBuf,
        /// Tick interval in seconds.
        #[arg(long, default_value = "1")]
        tick_interval: u64,
        /// Modbus TCP server address.
        #[arg(long, default_value = "0.0.0.0:8899")]
        modbus: SocketAddr,
        /// Output directory for recording frames.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Simulate a faulty dongle (Off, EmptyData, StaleData, GarbageData,
        /// DropConnection, Intermittent).
        #[arg(long, default_value = "Off")]
        dongle_misbehaviour: String,
    },
}

// ---------------------------------------------------------------------------
// Inverter configuration helpers
// ---------------------------------------------------------------------------

/// Battery sizes supported by GivEnergy (kWh).
const BATTERY_SIZES: [f64; 14] = [
    2.6, 3.4, 5.2, 6.8, 7.0, 8.2, 9.5, 10.2, 12.8, 13.6, 16.0, 17.0, 19.0, 20.4,
];

/// Find the nearest supported battery size to the requested value.
fn nearest_battery_size(requested: f64) -> f64 {
    *BATTERY_SIZES
        .iter()
        .min_by(|a, b| {
            ((*a - requested).abs())
                .partial_cmp(&((*b - requested).abs()))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(&9.5)
}

/// Max battery power (W) per inverter type. Mirrors sim-tauri commands.rs.
fn max_batt_w_for_inverter(inv_type: &str) -> f64 {
    match inv_type {
        "Gen1Hybrid" => 2500.0,
        "Gen2Hybrid" => 3600.0,
        "Gen3Hybrid" => 3600.0,
        "Gen3Hybrid8kW" => 8000.0,
        "Gen3Hybrid10kW" => 10000.0,
        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 2600.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        "AllInOne6" | "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        // Gateway: aggregates an All-in-One (6kW continuous) behind it.
        "Gateway12kW" => 6000.0,
        _ => 3600.0,
    }
}

/// Max AC power (W) per inverter type. Mirrors sim-tauri commands.rs.
fn max_ac_w_for_inverter(inv_type: &str) -> f64 {
    match inv_type {
        "Gen1Hybrid" | "Gen2Hybrid" | "Gen3Hybrid" => 5000.0,
        "Gen3Hybrid8kW" => 8000.0,
        "Gen3Hybrid10kW" => 10000.0,
        "Gen3Plus6kW" => 5000.0,
        "Gen3Plus4600" => 4600.0,
        "Gen3Plus3600" => 3600.0,
        "Gen3Plus6kW2" => 6000.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        "AllInOne6" | "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        // Gateway: aggregates an All-in-One (6kW AC) behind it.
        "Gateway12kW" => 6000.0,
        _ => 5000.0,
    }
}

/// DSP firmware version per inverter type.
fn dsp_firmware_for_inverter(inv_type: &str) -> u16 {
    match inv_type {
        "Gen1Hybrid" => 110,
        "Gen2Hybrid" => 230,
        "Gen3Hybrid" => 449,
        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 510,
        "ACCoupled" | "ACCoupled2" => 305,
        "ThreePhase" | "ThreePhase8kW" | "ThreePhase10kW" => 612,
        "ThreePhase11kW" => 11043,
        "AllInOne6" | "AllInOne" | "AllInOne5" => 1010,
        "AIO8kW" | "AIO10kW" => 1010,
        "AIOHybrid6kW" | "AIOHybrid8kW" | "AIOHybrid10kW" => 1010,
        _ => 449,
    }
}

/// Apply inverter-type configuration to a PlantState.
fn configure_inverter(state: &mut PlantState, inv_type: &str) {
    state.config.inverter_type = inv_type.to_string();
    state.config.max_ac_watts = max_ac_w_for_inverter(inv_type);
    // Seed the export limit at the standard UK EREC G98 default for this
    // family (3680 W single-phase, 6500 W three-phase wire ceiling, 0 W EMS).
    // See `sim_models::default_export_limit_w_for` for the source.
    state.inverter.export_limit_w = sim_models::default_export_limit_w_for(inv_type);
    state.inverter.dsp_firmware_version = dsp_firmware_for_inverter(inv_type);
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
        other => {
            // Try loading as a custom profile file
            let path = std::path::Path::new(other);
            if path.exists() {
                match load_custom_profile(path) {
                    Ok(profile) => profile,
                    Err(e) => {
                        tracing::warn!("Failed to load custom profile '{other}': {e}");
                        LoadProfile::Family
                    }
                }
            } else {
                LoadProfile::Family
            }
        }
    }
}

/// Load a custom load profile from a YAML file.
/// Format: list of `{hour: 0.0, watts: 200}` entries.
fn load_custom_profile(path: &std::path::Path) -> Result<LoadProfile, Box<dyn std::error::Error>> {
    let yaml = std::fs::read_to_string(path)?;
    let entries: Vec<LoadProfileEntry> = serde_yaml::from_str(&yaml)?;
    let mut points: Vec<(f64, f64)> = entries.into_iter().map(|e| (e.hour, e.watts)).collect();
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(LoadProfile::Custom(points))
}

#[derive(serde::Deserialize)]
struct LoadProfileEntry {
    hour: f64,
    watts: f64,
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
    let w = sim_core::parse_weather_from_str(s);
    // Return None only if it looks like garbage; our parser defaults to Clear
    // so check if the input matches a known variant
    match s.to_lowercase().as_str() {
        "clear" | "partlycloudy" | "partly-cloudy" | "partly_cloudy" | "overcast" | "storm" => {
            Some(w)
        }
        _ => None,
    }
}

/// Translate a Modbus register write into a simulation Command.
fn modbus_command_to_sim(cmd: &sim_modbus::ModbusCommand) -> Option<Command> {
    match cmd.address {
        20 => Some(Command::SetEnableChargeTarget(cmd.value != 0)),
        27 => {
            let mode = match cmd.value {
                1 => sim_models::InverterMode::Eco,
                _ => sim_models::InverterMode::Normal,
            };
            Some(Command::SetInverterMode(mode))
        }
        29 => {
            if cmd.value == 0 {
                Some(Command::CancelCalibration)
            } else {
                Some(Command::StartCalibration { module: None })
            }
        }
        50 => Some(Command::SetActivePowerRate(cmd.value as f64)),
        110 => Some(Command::SetMinSoc(cmd.value as f64)),
        111 => Some(Command::SetBatteryChargeLimit(cmd.value as f64)),
        166 => Some(Command::SetEnableRtc(cmd.value != 0)),
        112 => Some(Command::SetBatteryDischargeLimit(cmd.value as f64)),
        313 | 1110 => Some(Command::SetBatteryChargeLimit(cmd.value as f64)),
        314 | 1108 => Some(Command::SetBatteryDischargeLimit(cmd.value as f64)),
        163 => {
            if cmd.value == 100 {
                Some(Command::InverterReboot)
            } else {
                None
            }
        }
        199 => Some(Command::SetEnableInverterParallelMode(cmd.value != 0)),
        311 => Some(Command::SetExportPriority(cmd.value)),
        317 => Some(Command::SetEnableEps(cmd.value != 0)),
        2040 => Some(Command::SetEmsEnable(cmd.value != 0)),
        // HR 318/319/320 (battery pause mode + single pause slot) are merged
        // into PlantState together by the write-loop reconciliation so a lone
        // HR 318 write doesn't clobber the start/end window. See
        // `enqueue_pause_slot_update`.
        318..=320 => None,
        // HR 1122: Three-phase force discharge enable
        1122 => Some(Command::SetInverterMode(if cmd.value != 0 {
            sim_models::InverterMode::ForceDischarge
        } else {
            sim_models::InverterMode::Eco
        })),
        // HR 1123: Three-phase force charge enable
        1123 => Some(Command::SetInverterMode(if cmd.value != 0 {
            sim_models::InverterMode::ForceCharge
        } else {
            sim_models::InverterMode::Eco
        })),
        // HR 35-40: system time (year, month, day, hour, minute, second)
        // Handled separately via time register accumulation in the tick loop
        35..=40 => None,
        100 => {
            // inverter_mode: 0=Normal, 1=Eco, 2=ForceCharge, 3=ForceDischarge, 4=ExportLimit
            let mode = match cmd.value {
                0 => Some(sim_models::InverterMode::Normal),
                1 => Some(sim_models::InverterMode::Eco),
                2 => Some(sim_models::InverterMode::ForceCharge),
                3 => Some(sim_models::InverterMode::ForceDischarge),
                4 => Some(sim_models::InverterMode::ExportLimit),
                _ => None,
            }?;
            Some(Command::SetInverterMode(mode))
        }
        102 => {
            // inverter_export_limit_w
            Some(Command::SetExportLimit(cmd.value as f64))
        }
        1063 => {
            // tph_hr_p_export_limit — three-phase / HV / AIO. Wire encoding
            // is C.deci (raw = watts × 10); convert back to user-friendly
            // watts for `state.inverter.export_limit_w`.
            Some(Command::SetExportLimit((cmd.value as f64) / 10.0))
        }
        2071 => {
            // ems_export_power_limit — EMS / EmsCommercial / Gateway. Raw
            // watts (C.uint16, no scaling).
            Some(Command::SetExportLimit(cmd.value as f64))
        }
        210 => {
            // battery_min_soc
            Some(Command::SetMinSoc(cmd.value as f64))
        }
        211 => {
            // battery_max_soc
            Some(Command::SetMaxSoc(cmd.value as f64))
        }
        602 => {
            // config_weather: 0=Clear, 1=PartlyCloudy, 2=Overcast, 3=Storm
            let w = match cmd.value {
                0 => Some(WeatherCondition::Clear),
                1 => Some(WeatherCondition::PartlyCloudy),
                2 => Some(WeatherCondition::Overcast),
                3 => Some(WeatherCondition::Storm),
                _ => None,
            }?;
            Some(Command::SetWeather(w))
        }
        _ => None,
    }
}

/// Parse a dongle misbehaviour mode string into an Arc<Mutex<DongleMisbehaviourMode>>.
fn parse_dongle_mode(
    s: &str,
) -> std::sync::Arc<std::sync::Mutex<sim_models::DongleMisbehaviourMode>> {
    use sim_models::DongleMisbehaviourMode;
    let mode = match s {
        "EmptyData" => DongleMisbehaviourMode::EmptyData,
        "StaleData" => DongleMisbehaviourMode::StaleData,
        "GarbageData" => DongleMisbehaviourMode::GarbageData,
        "DropConnection" => DongleMisbehaviourMode::DropConnection,
        "Intermittent" => DongleMisbehaviourMode::Intermittent,
        _ => DongleMisbehaviourMode::Off,
    };
    std::sync::Arc::new(std::sync::Mutex::new(mode))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giv_sim=info,sim_api=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Simulate {
            inverter,
            batteries,
            battery_size,
            soc,
            solar_peak,
            load_level,
            load_profile,
            weather,
            latitude,
            date,
            tick_interval,
            modbus,
            output,
            pv2_peak,
            fast,
            dongle_misbehaviour,
            inverter_temperature,
        } => {
            simulate(
                &inverter,
                batteries,
                battery_size,
                soc,
                solar_peak,
                load_level,
                &load_profile,
                &weather,
                latitude,
                &date,
                tick_interval,
                modbus,
                output.as_deref(),
                pv2_peak,
                fast,
                &dongle_misbehaviour,
                inverter_temperature,
            )
            .await
        }
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
            battery_count,
            dongle_misbehaviour,
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
                battery_count,
                &dongle_misbehaviour,
            )
            .await
        }
        Commands::Replay {
            recording,
            diff,
            format,
        } => replay_recording(&recording, diff.as_ref(), &format).await,
        Commands::Serve {
            config,
            tick_interval,
            modbus,
            output,
            dongle_misbehaviour,
        } => {
            serve_config(
                &config,
                tick_interval,
                modbus,
                output.as_deref(),
                &dongle_misbehaviour,
            )
            .await
        }
    }
}

/// Start a headless simulation from CLI parameters.
///
/// Creates a plant with the given configuration and starts ticking.
/// The Modbus TCP server starts automatically. Runs until Ctrl+C.
#[allow(clippy::too_many_arguments)]
async fn simulate(
    inv_type: &str,
    battery_count: usize,
    battery_size: f64,
    soc: f64,
    solar_peak: f64,
    load_level: f64,
    load_profile: &str,
    weather: &str,
    latitude: f64,
    date: &str,
    tick_interval: u64,
    modbus_addr: SocketAddr,
    output_dir: Option<&std::path::Path>,
    pv2_peak: f64,
    fast: bool,
    dongle_misbehaviour: &str,
    inverter_temperature: Option<f64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let battery_count = battery_count.clamp(1, 6);
    let soc = soc.clamp(0.0, 100.0);
    let actual_battery_size = nearest_battery_size(battery_size);

    if (actual_battery_size - battery_size).abs() > 0.01 {
        tracing::info!(
            "Battery size {battery_size} kWh rounded to nearest supported size: {actual_battery_size} kWh"
        );
    }

    let max_batt_w = max_batt_w_for_inverter(inv_type);
    let max_batt_kw = max_batt_w / 1000.0;
    let per_module_max_kw = max_batt_kw / battery_count as f64;
    // C-rate of 0.7 continuous for LFP modules
    let c_rate_max_kw = actual_battery_size * 0.7;
    let module_max_kw = per_module_max_kw.min(c_rate_max_kw).min(10.0);

    let start_date = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let start_ts = start_date.and_hms_opt(0, 0, 0).unwrap();

    // Build battery modules
    let mut state = PlantState::with_battery_count(start_ts, battery_count);
    for b in &mut state.batteries {
        b.soc_percent = soc;
        b.capacity_kwh = actual_battery_size;
        b.nominal_capacity_kwh = actual_battery_size;
        b.max_charge_kw = module_max_kw;
        b.max_discharge_kw = module_max_kw;
    }
    // Re-seed throughput + SOH for the actual capacity. `with_battery_count`
    // already ran the seed against the placeholder 9.5 kWh default; once the
    // user-specified size is in, recompute so the throughput register is
    // proportional to the real pack size.
    sim_models::seed_batteries_for_age(&mut state.batteries, sim_models::BATTERY_DEFAULT_AGE_YEARS);
    state.sync_battery_from_vec();
    state.sync_battery_from_vec();

    // Configure inverter
    configure_inverter(&mut state, inv_type);
    state.config.solar_peak_watts = solar_peak;
    state.config.latitude = latitude;
    state.config.tick_interval_secs = tick_interval;
    state.config.pv2_peak_watts = pv2_peak;
    state.weather = format!("{:?}", parse_weather(weather));

    // Apply load override if specified
    if load_level > 0.0 {
        state.load_override = Some(load_level);
    }

    // Pin inverter temperature if requested (bypasses the thermal model).
    if let Some(t) = inverter_temperature {
        state.inverter.temperature_override = Some(t);
        state.inverter.temperature_celsius = t.clamp(-10.0, 80.0);
    }

    let load_profile = parse_profile(load_profile);

    // Seed daily energy totals from 00:00 → `now` so daily registers read
    // realistic values immediately rather than climbing from zero. Stamps
    // `last_reset_date = now.date()` on the EnergyTracker so the engine doesn't
    // clobber the seed on its first tick.
    let seed_params = sim_core::EnergySeedParams {
        peak_w: solar_peak,
        pv2_peak_w: pv2_peak,
        latitude,
        weather_str: &state.weather,
        batteries: &state.batteries,
        max_ac_watts: state.config.max_ac_watts,
        battery_charge_limit_percent: state.battery_charge_limit_percent,
        battery_discharge_limit_percent: state.battery_discharge_limit_percent,
    };
    state.energy_totals = sim_core::seed_energy_totals_for_time_of_day(
        state.timestamp,
        load_profile.clone(),
        &seed_params,
    );
    let seed_date = state.timestamp.date();

    let initial_schedule = sim_models::Schedule::default();
    let devices: Vec<Box<dyn DeviceModel>> = vec![
        Box::new(sim_core::ScheduleEngine::new(initial_schedule.clone())),
        Box::new(SolarEngine::new(solar_peak, latitude)),
        Box::new(LoadEngine::new(load_profile)),
        // EvcEngine runs AFTER LoadEngine and BEFORE InverterEngine so the
        // inverter sees the combined household + EV demand and routes spare
        // solar/battery output to the EV first.
        Box::new(sim_core::EvcEngine::new()),
        Box::new(InverterEngine::new()),
        Box::new(FaultEngine::new()),
        Box::new(BatteryEngine::new()),
        Box::new(sim_core::EnergyTracker::new().with_last_reset_date(seed_date)),
    ];
    let mut engine = SimulationEngine::new(state, devices, tick_interval);
    // By default lock the sim clock to the host wall clock so the served
    // inverter time matches the computer's time (no drift). --fast keeps the
    // original fixed-step behaviour for quick day-burns.
    if !fast {
        engine.anchor_to_wall_clock(None);
    }
    let mut schedule_opt: Option<sim_models::Schedule> = Some(initial_schedule);

    let reg_cat = sim_registers::default_register_catalogue();
    let mut reg_store = sim_registers::RegisterStore::new(reg_cat);

    if let Some(dir) = output_dir {
        std::fs::create_dir_all(dir)?;
    }

    let mut recording: Vec<RecordingFrame> = Vec::new();

    let total_capacity = actual_battery_size * battery_count as f64;
    let max_ac_w = max_ac_w_for_inverter(inv_type);
    tracing::info!("Starting GivEnergy Plant Simulator");
    tracing::info!("  Inverter:       {inv_type} ({max_ac_w:.0}W AC, {max_batt_w:.0}W battery)");
    tracing::info!(
        "  Batteries:      {battery_count} × {actual_battery_size} kWh = {total_capacity:.1} kWh total"
    );
    tracing::info!("  Initial SOC:    {soc:.0}%");
    let pv2_info = if pv2_peak > 0.0 {
        format!(" + PV2 {pv2_peak:.0}W")
    } else {
        String::new()
    };
    tracing::info!("  Solar PV:       {solar_peak:.0}W peak{pv2_info}");
    if load_level > 0.0 {
        tracing::info!("  House load:     {load_level:.0}W (fixed override)");
    } else {
        tracing::info!("  House load:     profile-based");
    }
    tracing::info!("  Weather:        {:?}", parse_weather(weather));
    tracing::info!("  Latitude:       {latitude}°");
    tracing::info!("  Start date:     {date}");
    tracing::info!("  Tick interval:  {tick_interval}s");
    if let Some(t) = inverter_temperature {
        tracing::info!("  Inv temp:       {t:.0}°C (pinned, thermal model off)");
    }
    tracing::info!("  Modbus server:  {modbus_addr}");

    // Initial register projection
    reg_store.project_from_state(&engine.state);

    // Start Modbus TCP server
    let store = Arc::new(tokio::sync::Mutex::new(reg_store.clone()));
    let server_store = store.clone();
    let battery_shared = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let batt_server = battery_shared.clone();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let dongle_mode = parse_dongle_mode(dongle_misbehaviour);
    let dongle_server = dongle_mode.clone();
    tokio::spawn(async move {
        if let Err(e) = sim_modbus::run_modbus_server(
            modbus_addr,
            server_store,
            cmd_tx,
            batt_server,
            dongle_server,
        )
        .await
        {
            tracing::error!("Modbus server error: {e}");
        }
    });
    tracing::info!("Modbus TCP server running on {modbus_addr}");
    tracing::info!("Simulation running... Press Ctrl+C to stop.");

    let mut tick_count: u64 = 0;
    let start = std::time::Instant::now();

    loop {
        // Drain Modbus write commands
        let mut sched_updates: std::collections::HashMap<u16, u16> =
            std::collections::HashMap::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            if is_schedule_register(cmd.address) || matches!(cmd.address, 318..=320) {
                sched_updates.insert(cmd.address, cmd.value);
            } else if let Some(sim_cmd) = modbus_command_to_sim(&cmd) {
                engine.enqueue(sim_cmd);
            }
        }

        // Apply schedule updates
        if !sched_updates.is_empty() {
            let mut sched = schedule_opt.clone().unwrap_or_default();
            apply_schedule_updates(&mut sched, &sched_updates);
            engine.enqueue(Command::SetSchedule(Box::new(sched.clone())));
            schedule_opt = Some(sched);
            enqueue_pause_slot_update(&mut engine, &sched_updates);
        }

        engine.tick();
        tick_count += 1;

        reg_store.project_from_state(&engine.state);
        if let Ok(mut ms) = store.try_lock() {
            ms.project_from_state(&engine.state);
            if let Some(ref sched) = schedule_opt {
                ms.project_schedule_for(sched, &engine.state.config.inverter_type);
            }
        }
        // Update battery snapshot for Modbus BMS reads
        if let Ok(mut bs) = battery_shared.try_lock() {
            *bs = engine.state.batteries.clone();
        }

        recording.push(RecordingFrame {
            timestamp: engine.state.timestamp,
            plant_state: engine.state.clone(),
            register_snapshot: reg_store.snapshot(),
        });

        if tick_count.is_multiple_of(1000) {
            let elapsed = start.elapsed();
            let soc_now = engine.state.aggregate_soc();
            tracing::info!(
                "[{tick_count}] tick/s={:.0} SOC={soc_now:.1}% solar={:.0}W load={:.0}W grid={:.0}W batt={:.0}W",
                tick_count as f64 / elapsed.as_secs_f64().max(0.001),
                engine.state.solar.generation_w,
                engine.state.load.demand_w,
                engine.state.grid.power_w,
                engine.state.total_battery_power_kw() * 1000.0,
            );
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutdown requested. Saving output...");
                break;
            }
            // Real-time mode throttles to one tick per tick_interval seconds
            // (the clock is anchored to wall time, so the sleep only sets the
            // refresh cadence). --fast mode polls as tightly as possible.
            _ = tokio::time::sleep(if fast {
                std::time::Duration::from_millis(10)
            } else {
                std::time::Duration::from_secs(tick_interval.max(1))
            }) => {}
        }
    }

    let elapsed = start.elapsed();
    let final_soc = engine.state.aggregate_soc();
    tracing::info!(
        "Simulation ran for {:.1}s ({tick_count} ticks, avg {:.0} tick/s). Final SOC={final_soc:.1}%",
        elapsed.as_secs_f64(),
        tick_count as f64 / elapsed.as_secs_f64().max(0.001),
    );

    if let Some(dir) = output_dir {
        let jsonl_path = dir.join("simulate.jsonl");
        let mut f = std::fs::File::create(&jsonl_path)?;
        for frame in &recording {
            write_frame(&mut f, frame)?;
        }
        tracing::info!(
            "Recording: {} ({} frames)",
            jsonl_path.display(),
            recording.len()
        );

        let csv_path = dir.join("simulate.csv");
        let mut f = std::fs::File::create(&csv_path)?;
        write_csv(&mut f, &recording)?;
        tracing::info!("CSV traces: {}", csv_path.display());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_scenario(
    scenario_path: &Path,
    tick_interval: u64,
    date: &str,
    peak_watts: f64,
    latitude: f64,
    profile: &str,
    weather: &str,
    output_dir: Option<&std::path::Path>,
    modbus: Option<SocketAddr>,
    battery_count: usize,
    dongle_misbehaviour: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let yaml = std::fs::read_to_string(scenario_path)?;
    let scen = parse_named_scenario(&yaml)?;

    let start_date = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
    let start_ts = start_date.and_hms_opt(0, 0, 0).unwrap();

    let mut state = PlantState::with_battery_count(start_ts, battery_count);
    state.config.solar_peak_watts = peak_watts;
    state.config.latitude = latitude;
    state.config.tick_interval_secs = tick_interval;
    state.weather = format!("{:?}", parse_weather(weather));

    let solar = SolarEngine::new(peak_watts, latitude);

    let load_profile = parse_profile(profile);

    // Order: Solar → Load → Inverter → Faults → Battery → EVC → EnergyTracker
    let initial_schedule = sim_models::Schedule::default();
    let devices: Vec<Box<dyn DeviceModel>> = vec![
        Box::new(sim_core::ScheduleEngine::new(initial_schedule.clone())),
        Box::new(solar),
        Box::new(LoadEngine::new(load_profile)),
        Box::new(sim_core::EvcEngine::new()),
        Box::new(InverterEngine::new()),
        Box::new(FaultEngine::new()),
        Box::new(BatteryEngine::new()),
        Box::new(sim_core::EnergyTracker::new()),
    ];

    let mut engine = SimulationEngine::new(state, devices, tick_interval);
    let mut schedule_opt: Option<sim_models::Schedule> = Some(initial_schedule);

    // Register store for Modbus and recording
    let reg_cat = sim_registers::default_register_catalogue();
    let mut reg_store = sim_registers::RegisterStore::new(reg_cat);

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
        "Running scenario '{}' ({} events, {} days, tick={}s, profile={}, weather={:?}, batteries={})",
        scen.name,
        scen.events.len(),
        scen.days,
        tick_interval,
        profile,
        parse_weather(weather),
        battery_count,
    );

    // Initial register projection so Modbus clients see non-zero values immediately.
    reg_store.project_from_state(&engine.state);

    // Optional: launch Modbus server in background
    let (modbus_store, mut modbus_rx) = if let Some(addr) = modbus {
        let store = std::sync::Arc::new(tokio::sync::Mutex::new(reg_store.clone()));
        let server_store = store.clone();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let dongle_mode = parse_dongle_mode(dongle_misbehaviour);
        let dongle_server = dongle_mode.clone();
        tokio::spawn(async move {
            if let Err(e) = sim_modbus::run_modbus_server(
                addr,
                server_store,
                cmd_tx,
                Arc::new(tokio::sync::Mutex::new(Vec::new())),
                dongle_server,
            )
            .await
            {
                tracing::error!("Modbus server error: {e}");
            }
        });
        tracing::info!("Modbus TCP server starting on {addr}");
        (Some(store), Some(cmd_rx))
    } else {
        (None, None)
    };

    // Run ticks, applying scenario events at matching times (repeat for each day)
    let num_days = scen.days.max(1);
    let mut time_regs: [Option<u16>; 6] = [None; 6];

    // Initial register projection so Modbus clients see non-zero values immediately.
    reg_store.project_from_state(&engine.state);

    for day in 0..num_days {
        let day_offset = chrono::TimeDelta::days(day as i64);
        let day_label = if num_days > 1 {
            format!(" (day {})", day + 1)
        } else {
            String::new()
        };

        for (time, event) in &scen.events {
            let target = start_date.and_time(*time) + day_offset;
            while engine.state.timestamp < target {
                // Drain Modbus write commands
                if let Some(ref mut rx) = modbus_rx {
                    let mut sched_updates: std::collections::HashMap<u16, u16> =
                        std::collections::HashMap::new();
                    while let Ok(cmd) = rx.try_recv() {
                        if is_schedule_register(cmd.address) || matches!(cmd.address, 318..=320) {
                            sched_updates.insert(cmd.address, cmd.value);
                        } else if let Some(sim_cmd) = modbus_command_to_sim(&cmd) {
                            engine.enqueue(sim_cmd);
                        }
                    }
                    if !sched_updates.is_empty() {
                        let mut sched = schedule_opt.clone().unwrap_or_default();
                        apply_schedule_updates(&mut sched, &sched_updates);
                        engine.enqueue(Command::SetSchedule(Box::new(sched.clone())));
                        schedule_opt = Some(sched);
                        enqueue_pause_slot_update(&mut engine, &sched_updates);
                    }
                }

                engine.tick();

                // Project state into registers and record frame
                reg_store.project_from_state(&engine.state);
                if let Some(ref modbus_store) = modbus_store {
                    if let Ok(mut ms) = modbus_store.try_lock() {
                        ms.project_from_state(&engine.state);
                    }
                }
                recording.push(RecordingFrame {
                    timestamp: engine.state.timestamp,
                    plant_state: engine.state.clone(),
                    register_snapshot: reg_store.snapshot(),
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
            if let Some(weather_str) = &event.weather {
                if let Some(w) = _parse_weather_cmd(weather_str) {
                    engine.enqueue(Command::SetWeather(w));
                }
            }

            // Tick once to let the event take effect
            // Also drain any pending Modbus commands
            if let Some(ref mut rx) = modbus_rx {
                let mut sched_updates: std::collections::HashMap<u16, u16> =
                    std::collections::HashMap::new();
                while let Ok(cmd) = rx.try_recv() {
                    match cmd.address {
                        35 => time_regs[0] = Some(cmd.value),
                        36 => time_regs[1] = Some(cmd.value),
                        37 => time_regs[2] = Some(cmd.value),
                        38 => time_regs[3] = Some(cmd.value),
                        39 => time_regs[4] = Some(cmd.value),
                        40 => time_regs[5] = Some(cmd.value),
                        _ => {}
                    }
                    if is_schedule_register(cmd.address) || matches!(cmd.address, 318..=320) {
                        sched_updates.insert(cmd.address, cmd.value);
                    } else if let Some(sim_cmd) = modbus_command_to_sim(&cmd) {
                        engine.enqueue(sim_cmd);
                    }
                }
                if !sched_updates.is_empty() {
                    let mut sched = schedule_opt.clone().unwrap_or_default();
                    apply_schedule_updates(&mut sched, &sched_updates);
                    engine.enqueue(Command::SetSchedule(Box::new(sched.clone())));
                    schedule_opt = Some(sched);
                    enqueue_pause_slot_update(&mut engine, &sched_updates);
                }
                if time_regs.iter().all(|r| r.is_some()) {
                    let y = time_regs[0].unwrap() as i32;
                    let m = time_regs[1].unwrap() as u32;
                    let d = time_regs[2].unwrap() as u32;
                    let h = time_regs[3].unwrap() as u32;
                    let min = time_regs[4].unwrap() as u32;
                    let s = time_regs[5].unwrap() as u32;
                    if let Some(dt) = chrono::NaiveDate::from_ymd_opt(y, m, d)
                        .and_then(|date| date.and_hms_opt(h, min, s))
                    {
                        engine.enqueue(Command::SetSimulationTime(dt));
                    }
                    time_regs = [None; 6];
                }
            }
            engine.tick();
            reg_store.project_from_state(&engine.state);
            if let Some(ref modbus_store) = modbus_store {
                if let Ok(mut ms) = modbus_store.try_lock() {
                    ms.project_from_state(&engine.state);
                }
            }
            recording.push(RecordingFrame {
                timestamp: engine.state.timestamp,
                plant_state: engine.state.clone(),
                register_snapshot: reg_store.snapshot(),
            });

            // Check assertions
            if let Some(expect) = &event.expect {
                let time_str = format!("{}{day_label}", time);
                match sim_scenarios::check_assertions(expect, &engine.state) {
                    Ok(()) => {
                        tracing::info!(
                            "[{}] ✓ assertions passed (SOC={:.1}%)",
                            time_str,
                            engine.state.aggregate_soc(),
                        );
                        scenario_result.passed += 1;
                        scenario_result.assertions.push(AssertionResult {
                            time: time_str,
                            passed: true,
                            messages: vec![],
                        });
                    }
                    Err(failures) => {
                        tracing::error!("[{}] ✗ assertion failures: {:?}", time_str, failures,);
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
    }

    tracing::info!("Scenario complete.");
    tracing::info!(
        "Final state: SOC={:.1}%, solar={:.0}W, load={:.0}W, grid={:.0}W",
        engine.state.aggregate_soc(),
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
        tracing::info!(
            "Recording: {} ({} frames)",
            jsonl_path.display(),
            recording.len()
        );

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

/// Replay a recording or diff two recordings.
async fn replay_recording(
    path: &PathBuf,
    diff_path: Option<&PathBuf>,
    format: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let frames = sim_storage::load_recording(path)?;
    tracing::info!(
        "Loaded recording: {} ({} frames)",
        path.display(),
        frames.len()
    );

    if let Some(diff) = diff_path {
        let other = sim_storage::load_recording(diff)?;
        tracing::info!(
            "Loaded diff recording: {} ({} frames)",
            diff.display(),
            other.len()
        );

        let diffs = sim_recording::diff_recordings(&frames, &other);

        if diffs.is_empty() {
            tracing::info!("Recordings are identical");
        } else {
            tracing::info!("Recordings differ at {} frame(s):", diffs.len());
            for idx in diffs.iter().take(10) {
                let a = &frames[*idx.min(&(frames.len() - 1))];
                let b = &other[*idx.min(&(other.len() - 1))];
                tracing::info!(
                    "  Frame {}: a={} (SOC={:.1}%) vs b={} (SOC={:.1}%)",
                    idx,
                    a.timestamp,
                    a.plant_state.aggregate_soc(),
                    b.timestamp,
                    b.plant_state.aggregate_soc(),
                );
            }
            if diffs.len() > 10 {
                tracing::info!("  ... and {} more", diffs.len() - 10);
            }
        }
    } else {
        match format {
            "csv" => {
                let csv_path = path.with_extension("replay.csv");
                let mut f = std::fs::File::create(&csv_path)?;
                write_csv(&mut f, &frames)?;
                tracing::info!("CSV output: {}", csv_path.display());
            }
            "json" => {
                let json = serde_json::to_string_pretty(&frames)?;
                let json_path = path.with_extension("replay.json");
                std::fs::write(&json_path, json)?;
                tracing::info!("JSON output: {}", json_path.display());
            }
            _ => {
                // Summary
                let first = frames
                    .first()
                    .map(|f| f.timestamp.to_string())
                    .unwrap_or_default();
                let last = frames
                    .last()
                    .map(|f| f.timestamp.to_string())
                    .unwrap_or_default();
                let first_soc = frames
                    .first()
                    .map(|f| f.plant_state.aggregate_soc())
                    .unwrap_or(0.0);
                let last_soc = frames
                    .last()
                    .map(|f| f.plant_state.aggregate_soc())
                    .unwrap_or(0.0);
                let first_solar = frames
                    .first()
                    .map(|f| f.plant_state.solar.generation_w)
                    .unwrap_or(0.0);
                let last_solar = frames
                    .last()
                    .map(|f| f.plant_state.solar.generation_w)
                    .unwrap_or(0.0);

                let totals = frames.last().map(|f| &f.plant_state.energy_totals);

                println!("=== Recording Summary ===");
                println!("Frames:     {}", frames.len());
                println!("Duration:   {} → {}", first, last);
                println!("SOC:        {:.1}% → {:.1}%", first_soc, last_soc);
                println!("Solar:      {:.0}W → {:.0}W", first_solar, last_solar);
                if let Some(et) = totals {
                    println!("Grid import:  {:.2} kWh", et.grid_import_kwh);
                    println!("Grid export:  {:.2} kWh", et.grid_export_kwh);
                    println!("Solar gen:    {:.2} kWh", et.solar_generation_kwh);
                    println!("Load cons:    {:.2} kWh", et.load_consumption_kwh);
                    println!("Batt charge:  {:.2} kWh", et.battery_charge_kwh);
                    println!("Batt disch:   {:.2} kWh", et.battery_discharge_kwh);
                }
            }
        }
    }

    Ok(())
}

/// Load a plant config JSON and run headless with Modbus server.
///
/// Creates the simulation engine from the saved plant state (including overrides),
/// starts the Modbus TCP server, and ticks indefinitely until Ctrl+C.
async fn serve_config(
    config_path: &std::path::Path,
    tick_interval: u64,
    modbus_addr: std::net::SocketAddr,
    output_dir: Option<&std::path::Path>,
    dongle_misbehaviour: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = std::fs::read_to_string(config_path)?;
    let cfg: PlantConfig = serde_json::from_str(&json)?;

    let mut state = cfg.plant;
    let mut schedule_opt = cfg.schedule;
    state.config.tick_interval_secs = tick_interval;

    let peak_watts = state.config.solar_peak_watts;
    let latitude = state.config.latitude;
    // Build devices — ScheduleEngine is always included so Modbus schedule
    // writes can take effect even when the initial config has no schedule.
    let initial_sched = schedule_opt.clone().unwrap_or_default();
    let devices: Vec<Box<dyn DeviceModel>> = vec![
        Box::new(sim_core::ScheduleEngine::new(initial_sched)),
        Box::new(SolarEngine::new(peak_watts, latitude)),
        Box::new(LoadEngine::new(LoadProfile::Family)),
        Box::new(sim_core::EvcEngine::new()),
        Box::new(InverterEngine::new()),
        Box::new(FaultEngine::new()),
        Box::new(BatteryEngine::new()),
        Box::new(sim_core::EnergyTracker::new()),
    ];

    let mut engine = SimulationEngine::new(state, devices, tick_interval);
    // Lock the served clock to the host wall clock — same rationale as `simulate`.
    engine.anchor_to_wall_clock(None);

    let reg_cat = sim_registers::default_register_catalogue();
    let mut reg_store = sim_registers::RegisterStore::new(reg_cat);

    if let Some(dir) = output_dir {
        std::fs::create_dir_all(dir)?;
    }

    let mut recording: Vec<sim_recording::RecordingFrame> = Vec::new();

    tracing::info!(
        "Serving plant config '{}' on {modbus_addr} (tick={tick_interval}s)",
        config_path.display(),
    );

    // Initial register projection so Modbus clients see non-zero values immediately.
    reg_store.project_from_state(&engine.state);

    let store = std::sync::Arc::new(tokio::sync::Mutex::new(reg_store.clone()));
    let server_store = store.clone();
    let battery_shared = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let batt_server = battery_shared.clone();
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let dongle_mode = parse_dongle_mode(dongle_misbehaviour);
    let dongle_server = dongle_mode.clone();
    tokio::spawn(async move {
        if let Err(e) = sim_modbus::run_modbus_server(
            modbus_addr,
            server_store,
            cmd_tx,
            batt_server,
            dongle_server,
        )
        .await
        {
            tracing::error!("Modbus server error: {e}");
        }
    });
    tracing::info!("Modbus TCP server running on {modbus_addr}");

    let mut tick_count: u64 = 0;
    let start = std::time::Instant::now();

    loop {
        // Phase 1: drain Modbus write commands, collecting schedule registers
        let mut sched_updates: std::collections::HashMap<u16, u16> =
            std::collections::HashMap::new();
        while let Ok(cmd) = cmd_rx.try_recv() {
            if is_schedule_register(cmd.address) {
                sched_updates.insert(cmd.address, cmd.value);
            } else if let Some(sim_cmd) = modbus_command_to_sim(&cmd) {
                engine.enqueue(sim_cmd);
            }
        }

        // Phase 2: apply schedule updates
        if !sched_updates.is_empty() {
            let mut sched = schedule_opt.clone().unwrap_or_default();
            // Charge slot 1 (HR 94-95)
            if let Some(&v) = sched_updates.get(&94) {
                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&95) {
                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
            }
            // Charge slot 2 (HR 31-32, GivTCP Gen3 aliases HR 243-244)
            if let Some(&v) = sched_updates.get(&31).or_else(|| sched_updates.get(&243)) {
                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&32).or_else(|| sched_updates.get(&244)) {
                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            // Discharge slot 1 (HR 56-57)
            if let Some(&v) = sched_updates.get(&56) {
                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&57) {
                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
            }
            // Discharge slot 2 (HR 44-45)
            if let Some(&v) = sched_updates.get(&44) {
                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&45) {
                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            // Global/slot target SOCs
            if let Some(&v) = sched_updates.get(&116) {
                sched.charge_target_soc = v as f64;
            }
            if let Some(&v) = sched_updates.get(&242) {
                sched.charge_target_soc = v as f64;
            }
            if let Some(&v) = sched_updates.get(&245) {
                sched.charge_target_soc_2 = v as f64;
            }
            if let Some(&v) = sched_updates.get(&272) {
                sched.discharge_target_soc = v as f64;
            }
            if let Some(&v) = sched_updates.get(&275) {
                sched.discharge_target_soc_2 = v as f64;
            }
            // Enable charge (HR 96) — 0 = disable, 1 = always-on
            if let Some(&v) = sched_updates.get(&96) {
                if v == 0 {
                    sched.charge_start = 0.0;
                    sched.charge_end = 0.0;
                    sched.enable_charge = false;
                } else {
                    sched.enable_charge = true;
                }
            }
            // Enable discharge (HR 59) — 0 = disable, 1 = always-on
            if let Some(&v) = sched_updates.get(&59) {
                if v == 0 {
                    sched.discharge_start = 0.0;
                    sched.discharge_end = 0.0;
                    sched.enable_discharge = false;
                } else {
                    sched.enable_discharge = true;
                }
            }
            // TPH mirrors
            if let Some(&v) = sched_updates.get(&1111) {
                sched.charge_target_soc = v as f64;
            }
            if let Some(&v) = sched_updates.get(&1112) {
                sched.enable_charge = v != 0;
            }
            if let Some(&v) = sched_updates.get(&1113) {
                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1114) {
                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1115) {
                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1116) {
                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1118) {
                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1119) {
                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1120) {
                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            if let Some(&v) = sched_updates.get(&1121) {
                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
            }
            engine.enqueue(Command::SetSchedule(Box::new(sched.clone())));
            schedule_opt = Some(sched);
        }

        engine.tick();
        tick_count += 1;

        reg_store.project_from_state(&engine.state);
        if let Ok(mut ms) = store.try_lock() {
            ms.project_from_state(&engine.state);
            if let Some(ref sched) = schedule_opt {
                ms.project_schedule_for(sched, &engine.state.config.inverter_type);
            }
        }
        // Update battery snapshot for Modbus BMS reads
        if let Ok(mut bs) = battery_shared.try_lock() {
            *bs = engine.state.batteries.clone();
        }

        recording.push(sim_recording::RecordingFrame {
            timestamp: engine.state.timestamp,
            plant_state: engine.state.clone(),
            register_snapshot: reg_store.snapshot(),
        });

        if tick_count.is_multiple_of(1000) {
            let elapsed = start.elapsed();
            let soc = engine.state.aggregate_soc();
            tracing::info!(
                "[{tick_count}] tick/s={:.0} SOC={soc:.1}% solar={:.0}W load={:.0}W grid={:.0}W",
                tick_count as f64 / elapsed.as_secs_f64().max(0.001),
                engine.state.solar.generation_w,
                engine.state.load.demand_w,
                engine.state.grid.power_w,
            );
        }

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Shutdown requested. Saving output...");
                break;
            }
            // Real-time mode: throttle to one tick per tick_interval seconds
            // (clock is anchored to wall time; this only sets refresh cadence).
            _ = tokio::time::sleep(std::time::Duration::from_secs(tick_interval.max(1))) => {}
        }
    }

    let elapsed = start.elapsed();
    tracing::info!(
        "Server ran for {:.1}s ({tick_count} ticks, avg {:.0} tick/s). Final SOC={:.1}%",
        elapsed.as_secs_f64(),
        tick_count as f64 / elapsed.as_secs_f64().max(0.001),
        engine.state.aggregate_soc(),
    );

    if let Some(dir) = output_dir {
        let base_name = config_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("serve");

        let jsonl_path = dir.join(format!("{base_name}.jsonl"));
        let mut f = std::fs::File::create(&jsonl_path)?;
        for frame in &recording {
            sim_recording::write_frame(&mut f, frame)?;
        }
        tracing::info!(
            "Recording: {} ({} frames)",
            jsonl_path.display(),
            recording.len()
        );

        let csv_path = dir.join(format!("{base_name}.csv"));
        let mut f = std::fs::File::create(&csv_path)?;
        sim_recording::write_csv(&mut f, &recording)?;
        tracing::info!("CSV traces: {}", csv_path.display());
    }

    Ok(())
}

/// Check if a register address is a schedule-related holding register.
///
/// HR 2071 (`ems_export_power_limit`) is excluded: the projection moved to
/// `project_from_state` and Modbus writes land directly in
/// `state.inverter.export_limit_w` via `SetExportLimit` (see
/// `crates/sim-modbus/src/lib.rs`). Routing it through the schedule accumulator
/// would race the GUI / client write with the schedule's window logic.
fn is_schedule_register(addr: u16) -> bool {
    matches!(
        addr,
        31..=32 | 44..=45 | 56..=57 | 59 | 94..=96 | 116
            | 242..=245 | 272 | 275
            | 246..=269 | 276..=299
            | 1109 | 1111..=1116 | 1118..=1121
            | 2062..=2070
            | 2044..=2061
    )
}

/// Convert HHMM register value to decimal hours.
/// Returns None for disabled (60) or invalid values.
fn hhmm_to_hours(val: u16) -> Option<f64> {
    if val == 60 {
        return None;
    }
    let hours = val / 100;
    let mins = val % 100;
    if mins > 59 || hours > 23 {
        return None;
    }
    Some(hours as f64 + mins as f64 / 60.0)
}

/// Apply schedule register updates to a Schedule struct.
/// Shared between run_scenario and serve_config.
/// Reconcile HR 318/319/320 (battery pause mode + single pause slot) writes
/// into one `SetBatteryPause` command, preserving whichever of mode/start/end
/// wasn't written this cycle. Mirrors the Tauri write-loop reconciliation so a
/// Modbus client can set the pause window one register at a time (as the
/// GivEnergy portal does) without earlier writes being lost.
fn enqueue_pause_slot_update(
    engine: &mut SimulationEngine,
    updates: &std::collections::HashMap<u16, u16>,
) {
    if updates.contains_key(&318) || updates.contains_key(&319) || updates.contains_key(&320) {
        let mode = updates
            .get(&318)
            .copied()
            .unwrap_or(engine.state.battery_pause_mode);
        let start = updates
            .get(&319)
            .copied()
            .unwrap_or(engine.state.battery_pause_slot_start);
        let end = updates
            .get(&320)
            .copied()
            .unwrap_or(engine.state.battery_pause_slot_end);
        engine.enqueue(Command::SetBatteryPause { mode, start, end });
    }
}

fn apply_schedule_updates(
    sched: &mut sim_models::Schedule,
    updates: &std::collections::HashMap<u16, u16>,
) {
    // Charge slot 1 (HR 94-95)
    if let Some(&v) = updates.get(&94) {
        sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&95) {
        sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
    }
    // Charge slot 2 (HR 31-32, GivTCP Gen3 aliases HR 243-244)
    if let Some(&v) = updates.get(&31).or_else(|| updates.get(&243)) {
        sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&32).or_else(|| updates.get(&244)) {
        sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    // Discharge slot 1 (HR 56-57)
    if let Some(&v) = updates.get(&56) {
        sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&57) {
        sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
    }
    // Discharge slot 2 (HR 44-45)
    if let Some(&v) = updates.get(&44) {
        sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&45) {
        sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    // Charge target SOCs
    if let Some(&v) = updates.get(&116).or_else(|| updates.get(&242)) {
        sched.charge_target_soc = v as f64;
    }
    if let Some(&v) = updates.get(&245) {
        sched.charge_target_soc_2 = v as f64;
    }
    if let Some(&v) = updates.get(&272) {
        sched.discharge_target_soc = v as f64;
    }
    if let Some(&v) = updates.get(&275) {
        sched.discharge_target_soc_2 = v as f64;
    }
    // Charge slot 3-10 (HR 246-268, alternating start/end)
    if let Some(&v) = updates.get(&246) {
        sched.charge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&247) {
        sched.charge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&248) {
        sched.charge_target_soc_3 = v as f64;
    }
    if let Some(&v) = updates.get(&249) {
        sched.charge_start_4 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&250) {
        sched.charge_end_4 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&251) {
        sched.charge_target_soc_4 = v as f64;
    }
    if let Some(&v) = updates.get(&252) {
        sched.charge_start_5 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&253) {
        sched.charge_end_5 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&254) {
        sched.charge_target_soc_5 = v as f64;
    }
    if let Some(&v) = updates.get(&255) {
        sched.charge_start_6 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&256) {
        sched.charge_end_6 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&257) {
        sched.charge_target_soc_6 = v as f64;
    }
    if let Some(&v) = updates.get(&258) {
        sched.charge_start_7 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&259) {
        sched.charge_end_7 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&260) {
        sched.charge_target_soc_7 = v as f64;
    }
    if let Some(&v) = updates.get(&261) {
        sched.charge_start_8 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&262) {
        sched.charge_end_8 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&263) {
        sched.charge_target_soc_8 = v as f64;
    }
    if let Some(&v) = updates.get(&264) {
        sched.charge_start_9 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&265) {
        sched.charge_end_9 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&266) {
        sched.charge_target_soc_9 = v as f64;
    }
    if let Some(&v) = updates.get(&267) {
        sched.charge_start_10 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&268) {
        sched.charge_end_10 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&269) {
        sched.charge_target_soc_10 = v as f64;
    }
    // Discharge slot 3-10 (HR 276-298, alternating start/end)
    if let Some(&v) = updates.get(&276) {
        sched.discharge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&277) {
        sched.discharge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&278) {
        sched.discharge_target_soc_3 = v as f64;
    }
    if let Some(&v) = updates.get(&279) {
        sched.discharge_start_4 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&280) {
        sched.discharge_end_4 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&281) {
        sched.discharge_target_soc_4 = v as f64;
    }
    if let Some(&v) = updates.get(&282) {
        sched.discharge_start_5 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&283) {
        sched.discharge_end_5 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&284) {
        sched.discharge_target_soc_5 = v as f64;
    }
    if let Some(&v) = updates.get(&285) {
        sched.discharge_start_6 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&286) {
        sched.discharge_end_6 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&287) {
        sched.discharge_target_soc_6 = v as f64;
    }
    if let Some(&v) = updates.get(&288) {
        sched.discharge_start_7 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&289) {
        sched.discharge_end_7 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&290) {
        sched.discharge_target_soc_7 = v as f64;
    }
    if let Some(&v) = updates.get(&291) {
        sched.discharge_start_8 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&292) {
        sched.discharge_end_8 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&293) {
        sched.discharge_target_soc_8 = v as f64;
    }
    if let Some(&v) = updates.get(&294) {
        sched.discharge_start_9 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&295) {
        sched.discharge_end_9 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&296) {
        sched.discharge_target_soc_9 = v as f64;
    }
    if let Some(&v) = updates.get(&297) {
        sched.discharge_start_10 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&298) {
        sched.discharge_end_10 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&299) {
        sched.discharge_target_soc_10 = v as f64;
    }
    // Enable charge (HR 96) — 0 = disable, 1 = always-on
    if let Some(&v) = updates.get(&96) {
        if v == 0 {
            sched.charge_start = 0.0;
            sched.charge_end = 0.0;
            sched.enable_charge = false;
        } else {
            sched.enable_charge = true;
        }
    }
    // Enable discharge (HR 59) — 0 = disable, 1 = always-on
    if let Some(&v) = updates.get(&59) {
        if v == 0 {
            sched.discharge_start = 0.0;
            sched.discharge_end = 0.0;
            sched.enable_discharge = false;
        } else {
            sched.enable_discharge = true;
        }
    }
    // TPH mirrors
    if let Some(&v) = updates.get(&1111) {
        sched.charge_target_soc = v as f64;
    }
    if let Some(&v) = updates.get(&1112) {
        sched.enable_charge = v != 0;
    }
    if let Some(&v) = updates.get(&1113) {
        sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1114) {
        sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1115) {
        sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1116) {
        sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1118) {
        sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1119) {
        sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1120) {
        sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
    if let Some(&v) = updates.get(&1121) {
        sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
    }
}
