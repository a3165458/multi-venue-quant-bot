'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
    parseToolProtocol,
    classifyToolOutcome,
    extractJsonObject,
    validateResearchExperiments,
    isExplicitLiveApplyRequest
} = require('./quant_agent_protocol.js');

test('parses DeepSeek DSML tool calls and typed parameters', () => {
    const content = `好的，执行策略横向对比。
<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="compare_strategies">
<｜｜DSML｜｜parameter name="data_file" string="true">BTC-mainnet.csv</｜｜DSML｜｜parameter>
<｜｜DSML｜｜parameter name="capital" string="false">10000</｜｜DSML｜｜parameter>
<｜｜DSML｜｜parameter name="strategies" string="false">["grid", "dca", "trend"]</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>`;

    const parsed = parseToolProtocol(content);
    assert.equal(parsed.tool_calls.length, 1);
    assert.equal(parsed.tool_calls[0].name, 'compare_strategies');
    assert.deepEqual(parsed.tool_calls[0].arguments, {
        data_file: 'BTC-mainnet.csv',
        capital: 10000,
        strategies: ['grid', 'dca', 'trend']
    });
    assert.equal(parsed.content, '好的，执行策略横向对比。');
});

test('keeps supporting one-line TOOL_CALL fallback', () => {
    const parsed = parseToolProtocol('TOOL_CALL {"name":"run_backtest","arguments":{"strategy":"grid"}}');
    assert.equal(parsed.tool_calls[0].name, 'run_backtest');
    assert.equal(parsed.tool_calls[0].arguments.strategy, 'grid');
});

test('treats a completed research run with no eligible candidate as a warning', () => {
    assert.equal(classifyToolOutcome({ status: 'no_candidate' }), 'warning');
    assert.equal(classifyToolOutcome({ status: 'ok' }), 'success');
    assert.equal(classifyToolOutcome({ status: 'error' }), 'error');
});

test('extracts and validates a bounded AI research plan', () => {
    const raw = '```json\n{"hypothesis":"wider grid","experiments":[{"strategy":"grid","data_file":"BTC.csv","start":"2026-01-01","end":"2026-02-01","params":"grid_count=8,investment=10,deviation=0.02"}]}\n```';
    const plan = extractJsonObject(raw);
    const validated = validateResearchExperiments(plan, {
        allowedDatasets: { 'BTC.csv': { start: '2026-01-01', end: '2026-03-01' } },
        maxExperiments: 3
    });
    assert.equal(validated.experiments.length, 1);
    assert.equal(validated.experiments[0].strategy, 'grid');
});

test('rejects unsafe or out-of-catalog AI experiments', () => {
    const validated = validateResearchExperiments({ experiments: [
        { strategy: 'grid', data_file: '../secret.csv', params: 'grid_count=8' },
        { strategy: 'trend', data_file: 'BTC.csv', params: 'fast_ma=5;fetch(attack)' },
        { strategy: 'custom_code', data_file: 'BTC.csv', params: 'x=1' }
    ] }, {
        allowedDatasets: { 'BTC.csv': { start: '2026-01-01', end: '2026-03-01' } },
        maxExperiments: 3
    });
    assert.equal(validated.experiments.length, 0);
    assert.equal(validated.rejected.length, 3);
});

test('rejects unknown and resource-exhausting strategy parameters', () => {
    const validated = validateResearchExperiments({ experiments: [
        { strategy: 'grid', data_file: 'BTC.csv', params: 'grid_count=999999999,investment=8,deviation=0.01' },
        { strategy: 'trend', data_file: 'BTC.csv', params: 'unknown_knob=1' }
    ] }, {
        allowedDatasets: { 'BTC.csv': { start: '2026-01-01', end: '2026-03-01' } },
        maxExperiments: 3
    });
    assert.equal(validated.experiments.length, 0);
    assert.deepEqual(validated.rejected.map((row) => row.reason), ['invalid_params', 'invalid_params']);
});

test('filters AI experiments against the current backend live notional cap', () => {
    const validated = validateResearchExperiments({ experiments: [
        { strategy: 'trend', data_file: 'BTC.csv', params: 'fast_ma=7,slow_ma=50,notional=64' },
        { strategy: 'trend', data_file: 'BTC.csv', params: 'fast_ma=7,slow_ma=50,notional=65' },
        { strategy: 'dca', data_file: 'BTC.csv', params: 'interval=4,amount=5,dip_threshold=2' }
    ] }, {
        allowedDatasets: { 'BTC.csv': { start: '2026-01-01', end: '2026-03-01' } },
        maxExperiments: 3,
        livePolicy: { maxNotionalUsd: 64.11, allowedStrategies: ['grid', 'trend'] }
    });
    assert.equal(validated.experiments.length, 1);
    assert.equal(validated.experiments[0].params, 'fast_ma=7,slow_ma=50,notional=64');
    assert.deepEqual(validated.rejected.map((row) => row.reason), ['live_notional_cap', 'not_live_allowlisted']);
});

test('routes only explicit positive live requests to the approval flow', () => {
    assert.equal(isExplicitLiveApplyRequest('把当前验证策略上线实盘'), true);
    assert.equal(isExplicitLiveApplyRequest('确认应用到实盘'), true);
    assert.equal(isExplicitLiveApplyRequest('不要上线实盘，只做回测'), false);
    assert.equal(isExplicitLiveApplyRequest('这个策略能上线吗？'), false);
});
