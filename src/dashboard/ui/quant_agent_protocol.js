(function (root, factory) {
    var api = factory();
    if (typeof module === 'object' && module.exports) module.exports = api;
    if (root) root.QuantAgentProtocol = api;
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
    'use strict';

    function decodeText(value) {
        return String(value || '')
            .replace(/&quot;/g, '"').replace(/&#39;|&apos;/g, "'")
            .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');
    }

    function typedValue(raw, isString) {
        var text = decodeText(raw).trim();
        if (isString) return text;
        try { return JSON.parse(text); } catch (e) {
            if (text === 'true') return true;
            if (text === 'false') return false;
            var number = Number(text);
            return Number.isFinite(number) ? number : text;
        }
    }

    function parseToolProtocol(content) {
        var source = String(content || '');
        var toolCalls = [];
        var dsml = '(?:\\|\\||｜｜)DSML(?:\\|\\||｜｜)';
        var invokeRe = new RegExp('<' + dsml + 'invoke\\s+name="([^"]+)"[^>]*>([\\s\\S]*?)<\\/' + dsml + 'invoke>', 'g');
        var invoke;
        var invokeIndex = 0;
        while ((invoke = invokeRe.exec(source)) !== null) {
            var args = {};
            var paramRe = new RegExp('<' + dsml + 'parameter\\s+name="([^"]+)"(?:\\s+string="(true|false)")?[^>]*>([\\s\\S]*?)<\\/' + dsml + 'parameter>', 'g');
            var param;
            while ((param = paramRe.exec(invoke[2])) !== null) {
                args[param[1]] = typedValue(param[3], param[2] !== 'false');
            }
            toolCalls.push({ id: 'dsml_' + invokeIndex++, name: invoke[1], arguments: args });
        }

        var lines = source.split(/\r?\n/);
        var finalText = source;
        lines.forEach(function (line, idx) {
            var match = line.match(/^\s*TOOL_CALL\s+(\{.*\})\s*$/);
            if (match) {
                try {
                    var parsed = JSON.parse(match[1]);
                    toolCalls.push({ id: 'text_' + idx, name: parsed.name, arguments: parsed.arguments || {} });
                } catch (e) { /* malformed fallback is left visible */ }
            }
            var finalMatch = line.match(/^\s*FINAL\s+([\s\S]*)$/);
            if (finalMatch) finalText = finalMatch[1];
        });

        if (toolCalls.length) {
            var toolCallsBlock = new RegExp('<' + dsml + 'tool_calls[^>]*>[\\s\\S]*?<\\/' + dsml + 'tool_calls>', 'g');
            finalText = source.replace(toolCallsBlock, '').replace(/^\s*TOOL_CALL\s+\{.*\}\s*$/gm, '').trim();
        }
        return { role: 'assistant', content: finalText.trim(), tool_calls: toolCalls };
    }

    function classifyToolOutcome(result) {
        if (!result || result.status === 'error') return 'error';
        if (result.status === 'no_candidate') return 'warning';
        return 'success';
    }

    function isExplicitLiveApplyRequest(raw) {
        var text = String(raw || '').trim();
        if (!text) return false;
        var negative = /(不要|别|暂不|先不|无需|不能|不想).{0,16}(上线|实盘|应用)|\b(do not|don't|dont|not yet|without)\b.{0,24}\b(apply|live)\b/i;
        if (negative.test(text)) return false;
        var question = /(能|可以|是否|怎么|如何|为什么|为何).{0,12}(上线|实盘)|(?:上线|实盘).{0,6}(吗|么|？|\?)|\b(can|could|how|whether)\b.{0,24}\b(apply|live)\b/i;
        if (question.test(text)) return false;
        return /(上线实盘|实盘上线|确认上线|应用到实盘|写入实盘|启动实盘|apply\s+(?:to\s+)?live|go\s+live)/i.test(text);
    }

    function extractRequestedCandidateNumber(raw) {
        var text = String(raw || '');
        var match = text.match(/第\s*([1-9]\d*)\s*(?:个)?\s*(?:候选|方案|策略)/)
            || text.match(/(?:按照|采用|使用|选择)\s*(?:第\s*)?([1-9]\d*)\s*(?:号|个候选|个方案|进行)/)
            || text.match(/\b(?:candidate|option)\s*#?\s*([1-9]\d*)\b/i);
        if (!match) return null;
        var number = Number(match[1]);
        return Number.isInteger(number) && number <= 100 ? number : null;
    }

    function extractJsonObject(raw) {
        var text = String(raw || '').trim()
            .replace(/^```(?:json)?\s*/i, '').replace(/\s*```$/, '');
        var start = text.indexOf('{');
        var end = text.lastIndexOf('}');
        if (start < 0 || end <= start) return null;
        try { return JSON.parse(text.slice(start, end + 1)); } catch (e) { return null; }
    }

    function validStrategyParams(strategy, params) {
        if (!params) return true;
        var bounds = {
            grid: {
                grid_count: [2, 100], investment: [0.01, 1000000], investment_per_grid: [0.01, 1000000],
                deviation: [0.00001, 0.5], price_deviation: [0.00001, 0.5]
            },
            trend: {
                fast_ma: [1, 1000], slow_ma: [2, 2000], stop_loss: [0, 0.9], take_profit: [0, 5],
                trailing_stop: [0, 0.9], notional: [0.01, 1000000], adx_threshold: [0, 100],
                adx_period: [1, 1000], confirm_slope_min: [-1, 1], confirm_lookback: [1, 2000]
            },
            dca: { interval: [0.01, 8760], amount: [0.01, 1000000], dip_threshold: [0, 100] }
        }[strategy];
        if (!bounds) return false;
        var values = {};
        var integerKeys = { grid_count: true, fast_ma: true, slow_ma: true, adx_period: true, confirm_lookback: true };
        var valid = params.split(',').every(function (part) {
            var match = part.trim().match(/^([A-Za-z_][A-Za-z0-9_]*)=(-?(?:\d+(?:\.\d*)?|\.\d+))$/);
            if (!match || !Object.prototype.hasOwnProperty.call(bounds, match[1])
                || Object.prototype.hasOwnProperty.call(values, match[1])) return false;
            var value = Number(match[2]);
            values[match[1]] = value;
            return Number.isFinite(value) && (!integerKeys[match[1]] || Number.isInteger(value))
                && value >= bounds[match[1]][0] && value <= bounds[match[1]][1];
        });
        if (!valid) return false;
        return strategy !== 'trend' || values.fast_ma == null || values.slow_ma == null
            || values.fast_ma < values.slow_ma;
    }

    function numericParam(params, names) {
        var found = null;
        String(params || '').split(',').some(function (part) {
            var pair = part.trim().split('=');
            if (pair.length === 2 && names.indexOf(pair[0].trim()) >= 0) {
                found = Number(pair[1].trim());
                return true;
            }
            return false;
        });
        return found;
    }

    function validateResearchExperiments(plan, options) {
        options = options || {};
        var allowed = options.allowedDatasets || {};
        var limit = Math.max(0, Math.min(Number(options.maxExperiments) || 3, 3));
        var input = plan && Array.isArray(plan.experiments) ? plan.experiments : [];
        var valid = [];
        var rejected = [];
        var datePattern = /^\d{4}-\d{2}-\d{2}$/;
        var paramsPattern = /^[A-Za-z0-9_.=,+\-\s]*$/;
        var livePolicy = options.livePolicy || null;
        input.slice(0, limit).forEach(function (experiment, index) {
            var strategy = experiment && String(experiment.strategy || '').toLowerCase();
            var file = experiment && String(experiment.data_file || '');
            var meta = allowed[file];
            var params = experiment && String(experiment.params || '').trim();
            var start = experiment && String(experiment.start || (meta && meta.start) || '');
            var end = experiment && String(experiment.end || (meta && meta.end) || '');
            var reason = '';
            if (['grid', 'trend', 'dca'].indexOf(strategy) < 0) reason = 'unsupported_strategy';
            else if (!meta) reason = 'dataset_not_allowed';
            else if (params.length > 500 || !paramsPattern.test(params)) reason = 'unsafe_params';
            else if (!validStrategyParams(strategy, params)) reason = 'invalid_params';
            else if (livePolicy && Array.isArray(livePolicy.allowedStrategies)
                && livePolicy.allowedStrategies.indexOf(strategy) < 0) reason = 'not_live_allowlisted';
            else if (livePolicy && Number.isFinite(Number(livePolicy.maxNotionalUsd))) {
                var sizing = strategy === 'trend' ? numericParam(params, ['notional'])
                    : numericParam(params, ['investment', 'investment_per_grid']);
                if (!Number.isFinite(sizing) || sizing <= 0 || sizing > Number(livePolicy.maxNotionalUsd)) {
                    reason = 'live_notional_cap';
                }
            }
            if (!reason && livePolicy && strategy === 'grid') {
                var count = numericParam(params, ['grid_count']);
                var deviation = numericParam(params, ['deviation', 'price_deviation']);
                if (!Number.isFinite(count) || count < 4 || count > 40
                    || !Number.isFinite(deviation) || deviation < 0.001 || deviation > 0.05) {
                    reason = 'live_param_policy';
                }
            }
            if (!reason && livePolicy && strategy === 'trend') {
                var fast = numericParam(params, ['fast_ma']);
                var slow = numericParam(params, ['slow_ma']);
                if (!Number.isFinite(fast) || !Number.isFinite(slow) || fast < 2 || fast >= slow || slow > 500) {
                    reason = 'live_param_policy';
                }
            }
            if (!reason && (!datePattern.test(start) || !datePattern.test(end) || start > end)) reason = 'invalid_dates';
            else if (!reason && ((meta.start && start < meta.start) || (meta.end && end > meta.end))) reason = 'dates_out_of_range';
            if (reason) rejected.push({ index: index, reason: reason });
            else valid.push({ strategy: strategy, data_file: file, start: start, end: end, params: params });
        });
        if (input.length > limit) {
            for (var i = limit; i < input.length; i++) rejected.push({ index: i, reason: 'experiment_limit' });
        }
        return {
            hypothesis: plan && typeof plan.hypothesis === 'string' ? plan.hypothesis.slice(0, 500) : '',
            experiments: valid,
            rejected: rejected
        };
    }

    return {
        parseToolProtocol: parseToolProtocol,
        classifyToolOutcome: classifyToolOutcome,
        isExplicitLiveApplyRequest: isExplicitLiveApplyRequest,
        extractRequestedCandidateNumber: extractRequestedCandidateNumber,
        extractJsonObject: extractJsonObject,
        validateResearchExperiments: validateResearchExperiments
    };
});
