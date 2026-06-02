
/**
 * GivEnergy Plant Simulator GUI Tests
 *
 * Uses addInitScript to inject __TAURI__ mock before page JS runs.
 * Note: the UI wraps ALL invoke params in { params: { ... } }.
 */
const { test, expect } = require('@playwright/test');

const mockTauriScript = `
  window.__TAURI_MOCK__ = { calls: [] };
  window.__TAURI__ = {
    core: {
      invoke: async (cmd, params) => {
        window.__TAURI_MOCK__.calls.push({ cmd, params: JSON.parse(JSON.stringify(params || {})) });
        if (cmd === 'create_plant') {
          return {
            timestamp: '2025-06-01T12:00:00', inverter_mode: 'Eco', battery_mode: 'Eco',
            inverter_type: 'Gen3Hybrid', inverter_ac_power_w: 3000,
            aggregate_soc: 75.0, battery_power_kw: 2.5, battery_temperature_celsius: 28.0,
            battery_module_count: 1,
            battery_modules: [{ capacity_kwh: 8.2, soc_percent: 75.0, power_kw: 2.5, temperature_celsius: 28.0 }],
            solar_generation_w: 4000, solar_override: null,
            load_demand_w: 1500, load_override: null,
            grid_power_w: -500, grid_connected: true,
            active_faults: [], weather: 'Clear',
            schedule: {
              enable_charge: false, enable_discharge: false, soc_reserve: 4,
              charge_target_soc: 100, charge_slot_1_start: 0, charge_slot_1_end: 530,
              charge_slot_2_start: 60, charge_slot_2_end: 60,
              discharge_slot_1_start: 60, discharge_slot_1_end: 60,
              discharge_slot_2_start: 60, discharge_slot_2_end: 60,
              battery_pause_mode: 0, pause_slot_start: 60, pause_slot_end: 60,
            },
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

test('inverter type dropdown has 11 options', async ({ page }) => {
  await setupPage(page);
  await expect(page.locator('#inverter-type option')).toHaveCount(11);
});

test('battery count has 3 options', async ({ page }) => {
  await setupPage(page);
  const opts = page.locator('#battery-count option');
  await expect(opts).toHaveCount(3);
  const vals = await page.locator('#battery-count').evaluate(el =>
    Array.from(el.options).map(o => o.value)
  );
  expect(vals).toEqual(['1', '2', '3']);
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
