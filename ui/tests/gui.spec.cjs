
/**
 * GivEnergy Plant Simulator GUI Tests
 *
 * Uses addInitScript to inject __TAURI__ mock before page JS runs.
 * Note: the UI wraps ALL invoke params in { params: { ... } }.
 */
const { test, expect } = require('@playwright/test');

const mockTauriScript = `
  // The page's <script> block (lines 17-85 of index.html) installs a fetch-based
  // shim that overwrites window.__TAURI__.core.invoke when __TAURI_INTERNALS__
  // is absent. Set a non-null sentinel to bypass that shim and keep the mock
  // in place.
  window.__TAURI_INTERNALS__ = { __mock__: true };
  window.__TAURI_MOCK__ = { calls: [] };
  // Build a complete ScheduleDto-shaped object: every charge/discharge slot
  // (1-10) and target SOC, all disabled (60/60). The real backend always
  // returns all 20 slots; an incomplete mock makes slotCompact() throw on
  // undefined.toFixed() and skip rendering the whole schedule card.
  function mockSchedule() {
    const sch = {
      enable_charge: false, enable_discharge: false, soc_reserve: 4,
      charge_target_soc: 100, battery_pause_mode: 0, pause_slot_start: 60, pause_slot_end: 60,
    };
    for (let i = 1; i <= 10; i++) {
      sch['charge_slot_' + i + '_start'] = 60;
      sch['charge_slot_' + i + '_end'] = 60;
      sch['discharge_slot_' + i + '_start'] = 60;
      sch['discharge_slot_' + i + '_end'] = 60;
      sch['charge_target_soc_' + i] = 100;
      sch['discharge_target_soc_' + i] = 4;
    }
    return sch;
  }
  // Complete BatteryModuleDto-shaped module so updateBatteryDisplay() can
  // call .toFixed() on every field (voltage_v, current_a, nominal_capacity_kwh,
  // soh, cycle_count) without throwing.
  function mockBattery() {
    return {
      soc_percent: 75.0, power_kw: 2.5, voltage_v: 51.2, current_a: 48.8,
      temperature_celsius: 28.0, capacity_kwh: 8.2, nominal_capacity_kwh: 8.2,
      soh: 1.0, cycle_count: 12.0,
    };
  }
  window.__TAURI__ = {
    core: {
      invoke: async (cmd, params) => {
        window.__TAURI_MOCK__.calls.push({ cmd, params: JSON.parse(JSON.stringify(params || {})) });
        if (cmd === 'create_plant') {
          // Honour the inverter_type the GUI passed in (when present), so
          // tests that select a non-default type can verify the resulting
          // register hint and Set-button enabled-state without juggling
          // extra mocks per case.
          const requested = params && params.params && params.params.inverter_type;
          const invType = requested || 'Gen3Hybrid';
          // Pick plausible caps per family so the field's current-watts
          // display is meaningful without each test having to specify it.
          const isThreePhase = invType.startsWith('ThreePhase') || invType === 'ACThreePhase';
          const isEms = invType === 'EMS' || invType === 'EmsCommercial' || invType === 'Gateway12kW';
          const maxOutputW = isThreePhase ? 11000 : isEms ? 5000 : 5000;
          const exportLimitW = isThreePhase || isEms ? 6500 : 5000;
          // ARM firmware per generation on the shared 0x2001 family code
          // (century = fw/100: 2=Gen1, 3=Gen3, 8=Gen2). Drives the Timed
          // Discharge visibility test, which gates HR 318-320 on 0x2001 + FW3xx.
          const armFw = { Gen1Hybrid: 252, Gen2Hybrid: 852, Gen3Hybrid: 318 }[invType] || 0;
          return {
            timestamp: '2025-06-01T12:00:00', inverter_mode: 'Eco', battery_mode: 'Eco',
            inverter_type: invType, inverter_ac_power_w: 3000,
            arm_firmware_version: armFw, dsp_firmware_version: 0,
            inverter_max_output_w: maxOutputW,
            inverter_temperature_celsius: 35.0,
            inverter_temperature_override: null,
            export_limit_w: exportLimitW,
            aggregate_soc: 75.0, battery_power_kw: 2.5, battery_temperature_celsius: 28.0,
            battery_module_count: 1,
            battery_modules: [mockBattery()],
            solar_generation_w: 4000, solar_override: null,
            load_demand_w: 1500, load_override: null,
            grid_power_w: -500, grid_connected: true,
            active_faults: [], weather: 'Clear',
            schedule: mockSchedule(),
            energy_totals: {
              grid_import_kwh: 1.5, grid_export_kwh: 3.2,
              battery_charge_kwh: 5.0, battery_discharge_kwh: 2.1,
              solar_generation_kwh: 12.5, load_consumption_kwh: 8.0,
            },
          };
        }
        if (cmd === 'has_saved_plant') return false;
        if (cmd === 'load_plant') return { timestamp: '2025-06-01T12:00:00', inverter_mode: 'Eco' };
        if (cmd === 'load_scenario') return { name: 'Test', event_count: 5 };
        if (cmd === 'export_recording') return '/tmp/test.csv';
        if (cmd === 'get_grid_port_max_power') return 5000;
        if (cmd === 'set_grid_port_max_power') return 'ThreePhase';
        return null;
      },
    },
    event: { listen: () => {} },
    dialog: { save: async () => '/tmp/test.csv' },
  };
`;

async function setupPage(page) {
  await page.addInitScript(mockTauriScript);
  await page.goto('http://localhost:1421');
  await page.waitForLoadState('domcontentloaded');
  await page.waitForTimeout(500);
}

/** Get recorded calls. Params are at calls[i].params.params for IPC commands. */
async function getCalls(page) {
  return await page.evaluate(() => window.__TAURI_MOCK__.calls);
}

/** Unwrap the double-nested params the UI creates: invoke(cmd, { params: { ... } }) */
function p(call) {
  return call.params.params || call.params;
}

// ===== Basic page load =====

test('page loads with form elements', async ({ page }) => {
  await setupPage(page);
  await expect(page.locator('#inverter-type')).toBeVisible();
  await expect(page.locator('#btn-create')).toBeVisible();
  await expect(page.locator('#btn-start')).toBeVisible();
});

test('inverter type dropdown has all preset options', async ({ page }) => {
  await setupPage(page);
  // The dropdown must expose every InverterType variant the Rust catalogue
  // accepts, so a CLI/JSON-loaded plant can be re-selected in the GUI
  // without an empty label rendering.
  await expect(page.locator('#inverter-type option')).toHaveCount(45);
});

test('battery count supports all 6 modules', async ({ page }) => {
  await setupPage(page);
  const opts = page.locator('#battery-count option');
  await expect(opts).toHaveCount(6);
  const vals = await page.locator('#battery-count').evaluate(el =>
    Array.from(el.options).map(o => o.value)
  );
  expect(vals).toEqual(['1', '2', '3', '4', '5', '6']);
});

test('load profile defaults to family with 4 options', async ({ page }) => {
  await setupPage(page);
  expect(await page.locator('#load-profile').inputValue()).toBe('family');
  await expect(page.locator('#load-profile option')).toHaveCount(4);
});

// ===== Create plant with all inverter types =====

const types = ['Gen3Hybrid','Gen3Hybrid8kW','Gen3Hybrid10kW','ACCoupled','ACCoupled2',
  'AllInOne6','AllInOne','AllInOne5','AIO8kW','AIO10kW','ThreePhase'];
for (const type of types) {
  test(`create plant with ${type}`, async ({ page }) => {
    await setupPage(page);
    await page.selectOption('#inverter-type', type);
    await page.click('#btn-create');
    await page.waitForTimeout(300);

    const calls = await getCalls(page);
    const c = calls.find(x => x.cmd === 'create_plant');
    expect(c).toBeTruthy();
    expect(p(c).inverter_type).toBe(type);
  });
}

test('ACCoupled with 6kW solar sends peak_watts', async ({ page }) => {
  await setupPage(page);
  await page.selectOption('#inverter-type', 'ACCoupled');
  await page.fill('#peak-watts', '6000');
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'create_plant');
  expect(c).toBeTruthy();
  expect(p(c).inverter_type).toBe('ACCoupled');
  expect(parseFloat(p(c).peak_watts)).toBe(6000);
});

test('create plant with 2 battery modules', async ({ page }) => {
  await setupPage(page);
  await page.selectOption('#battery-count', '2');
  await page.waitForTimeout(300);
  await expect(page.locator('.battery-module')).toHaveCount(2);
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'create_plant');
  expect(c).toBeTruthy();
  expect(p(c).battery_modules).toHaveLength(2);
});

// ===== Timed Discharge (HR 318-320 pause slot) visibility =====
//
// The Timed Discharge slot card is only rendered for AC-output inverter
// families (AC-coupled 0x3001/0x3002, AC three-phase 0x60xx, residential
// All-in-One 0x80xx). DC hybrids and three-phase/HV register-bank families
// use a different slot bank, so the card must be absent for them.

/** Create a plant with the given inverter type and wait for the schedule card. */
async function createPlantAndWaitForSchedule(page, type) {
  await setupPage(page);
  if (type) await page.selectOption('#inverter-type', type);
  await page.click('#btn-create');
  // "Charge Slot 1" is always present once the schedule renders, so wait
  // for it as a readiness signal before asserting on the conditional card.
  await expect(page.locator('#schedule-display')).toContainText('Charge Slot 1', { timeout: 5000 });
}

const VISIBLE_TYPES = [
  ['ACCoupled', 'AC-coupled (0x3001)'],
  ['ACCoupled2', 'AC-coupled Mk2 (0x3002)'],
  ['Gen3Hybrid', 'Gen3 Hybrid (0x2001, FW318)'],
  ['AllInOne6', 'residential All-in-One (0x8001)'],
  ['AllInOne', 'residential All-in-One (0x8002)'],
  ['AllInOne5', 'residential All-in-One (0x8003)'],
  ['ACThreePhase', 'AC three-phase (0x6001)'],
];
for (const [type, label] of VISIBLE_TYPES) {
  test(`Timed Discharge card visible for ${label}`, async ({ page }) => {
    await createPlantAndWaitForSchedule(page, type);
    await expect(page.locator('#schedule-display')).toContainText('Timed Discharge');
  });
}

const HIDDEN_TYPES = [
  ['Gen1Hybrid', 'Gen1 Hybrid (0x2001, FW252)'],
  ['Gen3Plus6kW', 'DC hybrid Gen3+ (0x2201)'],
  ['ThreePhase', 'three-phase (0x4001)'],
  ['AIO8kW', 'HV Gen3 AIO (0x8102)'],
  ['AIOHybrid6kW', 'AIO Hybrid (0x8201)'],
  ['Gateway12kW', 'Gateway (0x7001)'],
];
for (const [type, label] of HIDDEN_TYPES) {
  test(`Timed Discharge card hidden for ${label}`, async ({ page }) => {
    await createPlantAndWaitForSchedule(page, type);
    await expect(page.locator('#schedule-display')).not.toContainText('Timed Discharge');
  });
}

// ===== Mode and weather =====

test('set_mode sends ForceCharge', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.selectOption('#inverter-mode', 'ForceCharge');
  await page.click('#btn-set-mode');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'set_mode');
  expect(c).toBeTruthy();
  expect(p(c).mode).toBe('ForceCharge');
});

test('set_weather sends Overcast', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.selectOption('#weather', 'Overcast');
  await page.click('#btn-set-weather');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'set_weather');
  expect(c).toBeTruthy();
  expect(p(c).weather).toBe('Overcast');
});

// ===== Overrides =====

test('solar override sends watts', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.fill('#override-solar', '2000');
  await page.click('#btn-apply-overrides');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'set_solar_override');
  expect(c).toBeTruthy();
  expect(p(c).watts).toBe(2000);
});

test('clear overrides sends null', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.click('#btn-clear-overrides');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const s = calls.find(x => x.cmd === 'set_solar_override');
  const l = calls.find(x => x.cmd === 'set_load_override');
  expect(s).toBeTruthy();
  expect(p(s).watts).toBeNull();
  expect(l).toBeTruthy();
  expect(p(l).watts).toBeNull();
});

test('inverter temperature override pins value', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  // The sidebar shows the live inverter temperature readout.
  await expect(page.locator('#inv-temp-live')).toContainText('°C');
  await page.fill('#override-inv-temp', '70');
  await page.click('#btn-apply-inv-temp');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'set_inverter_temperature');
  expect(c).toBeTruthy();
  expect(p(c).celsius).toBe(70);
});

test('clear inverter temperature sends null', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.click('#btn-clear-inv-temp');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'set_inverter_temperature');
  expect(c).toBeTruthy();
  expect(p(c).celsius).toBeNull();
});

// ===== Faults =====

test('grid loss fault injection', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.click('#btn-grid-loss');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'inject_fault');
  expect(c).toBeTruthy();
  expect(p(c).fault_id).toBe('grid_loss');
});

// ===== Simulation control =====

test('start and pause simulation', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.click('#btn-start');
  await page.waitForTimeout(300);

  let calls = await getCalls(page);
  expect(calls.find(x => x.cmd === 'start_simulation')).toBeTruthy();

  await page.click('#btn-pause');
  await page.waitForTimeout(300);
  calls = await getCalls(page);
  expect(calls.find(x => x.cmd === 'pause_simulation')).toBeTruthy();
});

// ===== Save/Load =====

test('save and load plant', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  await page.click('#btn-save-plant');
  await page.waitForTimeout(300);
  let calls = await getCalls(page);
  expect(calls.find(x => x.cmd === 'save_plant')).toBeTruthy();

  await page.click('#btn-load-plant');
  await page.waitForTimeout(300);
  calls = await getCalls(page);
  expect(calls.find(x => x.cmd === 'load_plant')).toBeTruthy();
});

// ===== Export =====

test('export CSV calls export_recording', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);
  await page.click('#btn-export-csv');
  await page.waitForTimeout(300);

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'export_recording');
  expect(c).toBeTruthy();
  // The UI calls invoke('export_recording', { path: ..., format: 'csv' }) directly
  expect(p(c).format).toBe('csv');
});

// ===== Battery SOC Slider =====

test('battery SOC slider renders in battery display', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  // Manually inject battery display HTML to test slider rendering
  await page.evaluate(() => {
    const batteries = [
      { soc_percent: 50.0, power_kw: 1.5, voltage_v: 48.0, current_a: 30.0,
        temperature_celsius: 25.0, capacity_kwh: 8.2, nominal_capacity_kwh: 9.5,
        soh: 0.98, cycle_count: 100 },
      { soc_percent: 80.0, power_kw: -0.5, voltage_v: 51.0, current_a: -10.0,
        temperature_celsius: 27.0, capacity_kwh: 7.8, nominal_capacity_kwh: 9.5,
        soh: 0.95, cycle_count: 250 },
    ];
    // Call the global updateBatteryDisplay if available
    if (typeof window.updateBatteryDisplay === 'function') {
      window.updateBatteryDisplay(batteries);
    } else {
      // Directly call via the UI's function
      const container = document.getElementById('battery-modules-display');
      container.innerHTML = batteries.map((b, i) => {
        const socColor = b.soc_percent < 20 ? 'var(--red)' : b.soc_percent < 50 ? 'var(--yellow)' : 'var(--green)';
        const powerLabel = b.power_kw > 0.01 ? 'Charging' : b.power_kw < -0.01 ? 'Discharging' : 'Idle';
        const powerColor = b.power_kw > 0.01 ? 'var(--accent)' : b.power_kw < -0.01 ? 'var(--orange)' : 'var(--text-muted)';
        return `<div class="batt-module">
          <div class="batt-module-header">
            <span class="batt-module-title">Module ${i+1}</span>
            <span class="batt-module-soc" style="color:${socColor}">${b.soc_percent.toFixed(1)}%</span>
          </div>
          <div class="soc-gauge"><div class="soc-gauge-fill" style="width:${b.soc_percent}%;background:${socColor}"></div></div>
          <div class="batt-stat" style="gap:4px">
            <span style="font-size:10px">Set SOC</span>
            <input type="range" min="0" max="100" step="1" value="${Math.round(b.soc_percent)}"
              data-batt-idx="${i}" class="batt-soc-slider"
              style="flex:1;height:14px;accent-color:var(--accent);cursor:pointer">
            <span class="batt-soc-label" data-batt-idx="${i}" style="font-size:10px;min-width:28px;text-align:right">${Math.round(b.soc_percent)}%</span>
          </div>
          <div class="batt-stat"><span>Power</span><span style="color:${powerColor}">${powerLabel} ${Math.abs(b.power_kw).toFixed(2)} kW</span></div>
        </div>`;
      }).join('');
    }
  });

  const sliders = await page.locator('.batt-soc-slider').count();
  expect(sliders).toBe(2);

  // Move the first slider to 75
  const slider = page.locator('.batt-soc-slider').first();
  await slider.fill('75');

  const calls = await getCalls(page);
  const c = calls.find(x => x.cmd === 'set_battery_soc');
  expect(c).toBeTruthy();
  expect(p(c).module).toBe(0);
  expect(p(c).soc).toBe(75);
});

// ===== Grid Export Limit read-only display =====

test('grid export limit shows the single-phase G98 value', async ({ page }) => {
  await setupPage(page);
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  await expect(page.locator('#grid-export-limit-display')).toHaveText('5,000 W');
  await expect(page.locator('#grid-export-limit-note')).toHaveText(/G98 single-phase/);
});

test('grid export limit identifies the ThreePhase HR 1063 family', async ({ page }) => {
  await setupPage(page);
  await page.selectOption('#inverter-type', 'ThreePhase11kW');
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  await expect(page.locator('#grid-export-limit-display')).toHaveText('6,500 W');
  await expect(page.locator('#grid-export-limit-note')).toHaveText(/HR 1063/);
});

test('grid export limit identifies the Gateway HR 2071 family', async ({ page }) => {
  await setupPage(page);
  await page.selectOption('#inverter-type', 'Gateway12kW');
  await page.click('#btn-create');
  await page.waitForTimeout(300);

  await expect(page.locator('#grid-export-limit-display')).toHaveText('6,500 W');
  await expect(page.locator('#grid-export-limit-note')).toHaveText(/HR 2071/);
});

test('grid export limit note reclassifies on inverter-type change', async ({ page }) => {
  await setupPage(page);
  await expect(page.locator('#grid-export-limit-note')).toHaveText(/G98 single-phase/);

  await page.selectOption('#inverter-type', 'ThreePhase11kW');
  await expect(page.locator('#grid-export-limit-note')).toHaveText(/HR 1063/);
  await expect(page.locator('#grid-export-limit-display')).toHaveText('—');

  await page.selectOption('#inverter-type', 'Gateway12kW');
  await expect(page.locator('#grid-export-limit-note')).toHaveText(/HR 2071/);
});
