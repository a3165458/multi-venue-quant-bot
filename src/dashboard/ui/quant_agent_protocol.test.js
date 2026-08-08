'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { parseToolProtocol, classifyToolOutcome } = require('./quant_agent_protocol.js');

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
