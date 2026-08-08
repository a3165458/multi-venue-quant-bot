// Quant Bot Agent — tool-using research agent powered by the user's AI API key.
// Loaded after ai.js. Uses form fields on the page + /api/backtest* tools.
(function () {
    'use strict';

    var MAX_STEPS = 16;
    var MAX_AI_RESEARCH_ROUNDS = 3;
    var MAX_AI_EXPERIMENTS_PER_ROUND = 3;
    var agentBusy = false;
    var abortFlag = false;
    var chatEl = null;
    var history = []; // OpenAI-style messages for multi-turn
    // 对话上下文（默认 100 万，对齐长上下文模型）；接近阈值时 compact
    var DEFAULT_CONTEXT_WINDOW = 1000000;
    var COMPACT_TRIGGER_RATIO = 0.72; // 超过 72% 自动压缩
    var COMPACT_KEEP_RECENT = 12;     // 压缩后保留最近消息条数（不含 system）
    var lastEstTokens = 0;
    var lastVerifiedBacktest = null;
    var activeRequestController = null;

    var TOOLS = [
        {
            type: 'function',
            function: {
                name: 'list_datasets',
                description: 'List available backtest CSV datasets with date ranges and candle counts. Call this first if the user has not chosen data.',
                parameters: { type: 'object', properties: {}, additionalProperties: false }
            }
        },
        {
            type: 'function',
            function: {
                name: 'get_workspace',
                description: 'Read current UI workspace: strategy, data file, date range, capital, params, goal.',
                parameters: { type: 'object', properties: {}, additionalProperties: false }
            }
        },
        {
            type: 'function',
            function: {
                name: 'set_workspace',
                description: 'Update workspace fields before running backtests. Only set fields you want to change.',
                parameters: {
                    type: 'object',
                    properties: {
                        strategy: { type: 'string', enum: ['grid', 'trend', 'dca'] },
                        data_file: { type: 'string' },
                        start: { type: 'string', description: 'YYYY-MM-DD' },
                        end: { type: 'string', description: 'YYYY-MM-DD' },
                        capital: { type: 'number' },
                        params: { type: 'string', description: 'key=value,key=value,...' },
                        goal: { type: 'string', enum: ['sharpe', 'return', 'drawdown', 'balanced'] }
                    },
                    additionalProperties: false
                }
            }
        },
        {
            type: 'function',
            function: {
                name: 'run_backtest',
                description: 'Run a REAL backtest on the bot engine. Returns verified metrics (return, sharpe, trades, drawdown). Always verify claims with this tool.',
                parameters: {
                    type: 'object',
                    properties: {
                        strategy: { type: 'string' },
                        data_file: { type: 'string' },
                        start: { type: 'string' },
                        end: { type: 'string' },
                        capital: { type: 'number' },
                        params: { type: 'string' }
                    },
                    additionalProperties: false
                }
            }
        },
        {
            type: 'function',
            function: {
                name: 'run_param_sweep',
                description: 'Local grid search over many parameter combos (no LLM). Returns leaderboard of verified results. Good for exploration before AI refinement.',
                parameters: {
                    type: 'object',
                    properties: {
                        strategy: { type: 'string' },
                        data_file: { type: 'string' },
                        start: { type: 'string' },
                        end: { type: 'string' },
                        capital: { type: 'number' },
                        goal: { type: 'string' },
                        mode: { type: 'string', enum: ['quick', 'full'] },
                        params: { type: 'string', description: 'optional baseline params' }
                    },
                    additionalProperties: false
                }
            }
        },
        {
            type: 'function',
            function: {
                name: 'compare_strategies',
                description: 'Run baseline-style backtests for grid and trend (and optional dca) with sensible defaults on the same data window. Returns side-by-side verified metrics.',
                parameters: {
                    type: 'object',
                    properties: {
                        data_file: { type: 'string' },
                        start: { type: 'string' },
                        end: { type: 'string' },
                        capital: { type: 'number' },
                        strategies: { type: 'array', items: { type: 'string', enum: ['grid', 'dca', 'trend'] } },
                        goal: { type: 'string', enum: ['balanced', 'sharpe', 'return', 'drawdown'] }
                    },
                    additionalProperties: false
                }
            }
        },
        {
            type: 'function',
            function: {
                name: 'research_strategies',
                description: 'Run a complete strategy research mission: read live/risk context, sweep the selected strategy universe, enforce a drawdown budget, rank verified candidates, and recommend one. Never applies live settings.',
                parameters: {
                    type: 'object',
                    properties: {
                        goal: { type: 'string', enum: ['balanced', 'sharpe', 'return', 'drawdown'] },
                        risk: { type: 'string', enum: ['conservative', 'balanced', 'aggressive'] },
                        universe: { type: 'string', enum: ['core', 'all', 'grid', 'trend'] }
                    },
                    additionalProperties: false
                }
            }
        },
        {
            type: 'function',
            function: {
                name: 'apply_to_live',
                description: 'Push strategy+params to the LIVE trading bot. DANGEROUS. Only after user explicitly asks to go live and results look acceptable.',
                parameters: {
                    type: 'object',
                    properties: {
                        strategy: { type: 'string' },
                        params: { type: 'string' },
                        confirm: { type: 'boolean', description: 'must be true' }
                    },
                    required: ['strategy', 'params', 'confirm'],
                    additionalProperties: false
                }
            }
        }
    ];

    function $(id) { return document.getElementById(id); }
    function val(id, fallback) {
        var el = $(id);
        if (!el) return fallback;
        var v = el.value;
        return v === undefined || v === null || v === '' ? fallback : v;
    }
    function num(id, fallback) {
        var n = parseFloat(val(id, fallback));
        return Number.isFinite(n) ? n : fallback;
    }

    function systemPrompt() {
        var lang = (localStorage.getItem('lighter-lang') || 'cn');
        var zh = lang === 'cn';
        var cfg = getProvider();
        return [
            zh
                ? '你是 Lighter Quant Bot 网页上的 Quant Agent 助手。'
                : 'You are the Quant Agent assistant inside Lighter Quant Bot.',
            zh
                ? '当前用户配置的模型 ID：' + (cfg.model || '（未填写）') + '；提供商：' + (cfg.provider || '—') + '。若被问“用什么模型”，直接如实回答，一两句即可，不要自我介绍长文。'
                : 'Configured model id: ' + (cfg.model || '(empty)') + '; provider: ' + (cfg.provider || '—') + '. If asked which model you use, answer briefly with that id.',
            zh
                ? '分流规则：闲聊/元问题（你好、你是谁、用什么模型、怎么用）→ 简短回答，不要调用工具，不要复述整份工作区。'
                : 'Routing: small talk / meta questions → short answer, NO tools, do not dump the full workspace.',
            zh
                ? '只有用户明确要求回测、扫参、对比策略、改参数、上线时，才调用工具。涉及收益/夏普/成交笔数必须用工具，禁止编造。'
                : 'Only call tools when the user asks for backtests, sweeps, comparisons, param changes, or live apply. Never invent metrics.',
            zh
                ? '研究任务时：先工具拿真数 → 再简短结论。趋势 notional 必须 < capital；apply_to_live 仅当用户明确要求且 confirm=true。'
                : 'For research: tools first, then brief conclusion. Trend notional must be < capital. apply_to_live only with explicit user request + confirm=true.',
            zh
                ? '默认用中文，简洁。'
                : 'Be concise. Prefer the user language.'
        ].join('\n');
    }

    /** 本地直答：不问模型、不走工具环，适合元问题 */
    function tryQuickReply(text) {
        var q = String(text || '').trim();
        if (!q) return null;
        var low = q.toLowerCase();
        var cfg = getProvider();
        var zh = (localStorage.getItem('lighter-lang') || 'cn') === 'cn';

        // 问模型
        if (/用什么模型|什么模型|哪个模型|which model|what model|model (are|do) you|当前模型/.test(low + q)) {
            if (!cfg.model) {
                return zh
                    ? '左侧「模型 ID」还是空的。填好后我就会用那个模型（例如 gpt-4o、mimo-v2.5-pro）。'
                    : 'No model id set in the left panel yet. Fill “Model ID” first.';
            }
            return zh
                ? '当前配置的模型是 **' + cfg.model + '**（提供商：' + cfg.provider + '）。\n请求会发到：' + (cfg.url || '（未填 API 地址）')
                : 'Configured model: **' + cfg.model + '** (provider: ' + cfg.provider + ').\nEndpoint: ' + (cfg.url || '(empty)');
        }

        // 你好
        if (/^(你好|您好|hi|hello|hey)[\s!！.。?？]*$/i.test(q)) {
            return zh
                ? '你好。需要回测/扫参/对比策略时直接说目标；问模型、配置之类也可以直接问。'
                : 'Hi. Ask for backtests/sweeps when needed, or simple config questions anytime.';
        }

        // 你是谁
        if (/你是谁|你是什么|who are you|what are you/.test(low + q)) {
            return zh
                ? '我是网页上的 Quant Agent：闲聊直接答；研究任务会调用回测等工具拿真实数据。'
                : 'I’m the on-page Quant Agent: short answers for chat; tools for real backtests.';
        }

        // 怎么用
        if (/怎么用|如何使用|how (do|to) use|help$|^帮助$/.test(low + q)) {
            return zh
                ? '1) 左侧填 API 地址 / 模型 / Key\n2) 选好数据与策略\n3) 中间用自然语言下任务，例如「用最新 BTC 数据优化网格」\n简单问题（用什么模型）我会直接答，不会跑一堆步骤。'
                : '1) Set API url / model / key on the left\n2) Pick data & strategy\n3) Type a research goal in chat\nSimple questions get a direct answer—no multi-step loop.';
        }

        return null;
    }

    function looksLikeResearchTask(text) {
        var q = String(text || '');
        return /回测|扫参|扫描|优化|对比|参数|网格|趋势|上线|apply|backtest|sweep|optim|compare|sharpe|收益|策略|run_|list_dataset|experiment|研究/i.test(q);
    }

    function nowTime() {
        var d = new Date();
        return [d.getHours(), d.getMinutes(), d.getSeconds()].map(function (x) {
            return String(x).padStart(2, '0');
        }).join(':');
    }

    function appendChat(role, html, cls, metaExtra) {
        if (!chatEl) chatEl = $('agent-chat');
        if (!chatEl) return;
        var empty = $('agent-chat-empty');
        if (empty) empty.style.display = 'none';
        var div = document.createElement('div');
        div.className = 'agent-msg agent-msg-' + role + (cls ? ' ' + cls : '');
        var meta = nowTime() + ' · ' + role + (metaExtra ? ' · ' + metaExtra : '');
        div.innerHTML = '<div class="agent-msg-meta"></div><div class="agent-msg-body"></div>';
        div.querySelector('.agent-msg-meta').textContent = meta;
        div.querySelector('.agent-msg-body').innerHTML = html;
        chatEl.appendChild(div);
        chatEl.scrollTop = chatEl.scrollHeight;
        return div;
    }

    function appendText(role, text, cls, metaExtra) {
        var esc = String(text == null ? '' : text)
            .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
        return appendChat(role, '<pre class="agent-pre">' + esc + '</pre>', cls, metaExtra);
    }

    function formatUsage(usage) {
        if (!usage || typeof usage !== 'object') return '';
        var pin = usage.prompt_tokens != null ? usage.prompt_tokens
            : (usage.input_tokens != null ? usage.input_tokens : null);
        var pout = usage.completion_tokens != null ? usage.completion_tokens
            : (usage.output_tokens != null ? usage.output_tokens : null);
        var tot = usage.total_tokens != null ? usage.total_tokens
            : (pin != null && pout != null ? pin + pout : null);
        var parts = [];
        if (pin != null) parts.push('in ' + pin);
        if (pout != null) parts.push('out ' + pout);
        if (tot != null) parts.push('Σ ' + tot);
        return parts.length ? 'tokens ' + parts.join(' · ') : '';
    }

    var TOOL_META = {
        list_datasets: { icon: '📁', label: '列出数据' },
        get_workspace: { icon: '🧭', label: '读取工作区' },
        set_workspace: { icon: '✏️', label: '更新工作区' },
        run_backtest: { icon: '📈', label: '运行回测' },
        run_param_sweep: { icon: '🔬', label: '参数扫描' },
        compare_strategies: { icon: '⚖️', label: '策略对比' },
        research_strategies: { icon: '🧪', label: '策略研究' },
        apply_to_live: { icon: '🚀', label: '应用到实盘' }
    };

    function escHtml(s) {
        return String(s == null ? '' : s)
            .replace(/&/g, '&amp;').replace(/</g, '&lt;')
            .replace(/>/g, '&gt;').replace(/"/g, '&quot;');
    }

    function summarizeToolResult(name, args, result, ok) {
        if (!ok) {
            return (result && (result.message || result.error)) || '调用失败';
        }
        if (name === 'run_backtest' && result) {
            var ret = Number(result.total_return_pct || 0);
            return (ret >= 0 ? '+' : '') + ret.toFixed(2) + '% · '
                + (result.total_trades || 0) + ' 笔 · Sharpe '
                + Number(result.sharpe_ratio || 0).toFixed(2);
        }
        if (name === 'run_param_sweep' && result) {
            return '扫 ' + (result.tested || 0) + ' 组 · 有成交 '
                + (result.with_trades || 0) + ' · 最优 '
                + (result.optimized_params || '—');
        }
        if (name === 'research_strategies' && result && result.status === 'no_candidate') {
            return '研究完成 · 无策略通过收益与风险门槛';
        }
        if (name === 'compare_strategies' && result) {
            var rows = result.ranked || [];
            return rows.map(function (row) {
                return row.strategy + ' ' + Number(row.metrics && row.metrics.total_return_pct || 0).toFixed(2) + '%';
            }).join(' / ') || '策略对比完成';
        }
        if (name === 'list_datasets' && result) {
            var n = (result.datasets && result.datasets.length) || 0;
            return n + ' 个数据集' + (result.default ? ' · 默认 ' + result.default : '');
        }
        if (name === 'get_workspace' || name === 'set_workspace') {
            return (result && result.strategy ? result.strategy : '')
                + (result && result.data_file ? ' · ' + result.data_file : '');
        }
        if (name === 'apply_to_live') {
            return (result && result.status) || '已提交';
        }
        var keys = args && typeof args === 'object' ? Object.keys(args) : [];
        return keys.length ? keys.slice(0, 3).join(', ') : '完成';
    }

    function bumpToolCount() {
        var rail = $('agent-tool-rail');
        var el = $('tool-hit-count');
        if (!rail || !el) return;
        el.textContent = String(rail.querySelectorAll('.tool-hit').length);
    }

    function appendTool(name, args, result, outcome) {
        outcome = typeof outcome === 'boolean' ? (outcome ? 'success' : 'error') : outcome;
        var ok = outcome !== 'error';
        var stateClass = outcome === 'warning' ? 'warn' : (ok ? 'ok' : 'err');
        var stateLabel = outcome === 'warning' ? 'NONE' : (ok ? 'OK' : 'ERR');
        var meta = TOOL_META[name] || { icon: '🛠', label: name };
        var summary = summarizeToolResult(name, args, result, ok);
        var argsStr = JSON.stringify(args || {}, null, 0);
        var resStr = typeof result === 'string' ? result : JSON.stringify(result, null, 2);
        if (resStr.length > 2500) resStr = resStr.slice(0, 2500) + '\n…';

        // 对话里：紧凑摘要卡 + 可展开原始 JSON
        var chatHtml = '<div class="tool-card ' + stateClass + '">'
            + '<div class="tool-name">' + escHtml(meta.icon + ' ' + meta.label) + '</div>'
            + '<div class="tool-args">' + escHtml(summary) + '</div>'
            + '<details><summary style="padding:6px 11px 8px;cursor:pointer;font-family:var(--font-mono);font-size:10px;color:var(--text-muted);">详情</summary>'
            + '<div class="tool-args"><code>' + escHtml(argsStr) + '</code></div>'
            + '<pre class="tool-out">' + escHtml(resStr) + '</pre></details></div>';
        appendChat('tool', chatHtml, outcome === 'warning' ? 'step-warn' : (ok ? 'step-ok' : 'step-err'));

        // 右侧时间线：结构化条目
        var rail = $('agent-tool-rail');
        if (rail) {
            var hit = document.createElement('div');
            hit.className = 'tool-hit ' + stateClass;
            hit.innerHTML =
                '<div class="th-icon">' + escHtml(meta.icon) + '</div>'
                + '<div class="th-main">'
                +   '<div class="th-name">' + escHtml(meta.label) + '</div>'
                +   '<div class="th-sum">' + escHtml(summary) + '</div>'
                +   '<span class="th-badge">' + stateLabel + '</span>'
                + '</div>'
                + '<div class="th-meta">' + nowTime() + '</div>'
                + '<details><summary>参数 / 原始返回</summary>'
                + '<div class="th-json">' + escHtml('args: ' + argsStr + '\n\n' + resStr) + '</div>'
                + '</details>';
            rail.appendChild(hit);
            rail.scrollTop = rail.scrollHeight;
            bumpToolCount();
        }
    }

    function setBusy(on) {
        agentBusy = on;
        var send = $('agent-send');
        var stop = $('agent-stop');
        if (send) send.disabled = on;
        if (stop) stop.style.display = on ? 'inline-flex' : 'none';
        var live = $('process-live');
        if (live) live.style.display = on ? 'inline-flex' : 'none';
        var missionRun = $('mission-run');
        if (missionRun) {
            missionRun.disabled = on;
            missionRun.textContent = on ? '研究中…' : '开始研究';
        }
    }

    function workspace() {
        return {
            strategy: val('bt-strategy', 'grid'),
            data_file: val('bt-data', ''),
            start: val('bt-start', ''),
            end: val('bt-end', ''),
            capital: num('bt-capital', 125),
            params: val('bt-params', ''),
            goal: val('ai-goal', 'sharpe')
        };
    }

    function applyWorkspace(patch) {
        if (!patch || typeof patch !== 'object') return workspace();
        if (patch.strategy && $('bt-strategy')) {
            $('bt-strategy').value = patch.strategy;
            $('bt-strategy').dispatchEvent(new Event('change'));
        }
        if (patch.data_file && $('bt-data')) {
            $('bt-data').value = patch.data_file;
            $('bt-data').dispatchEvent(new Event('change'));
        }
        if (patch.start && $('bt-start')) $('bt-start').value = patch.start;
        if (patch.end && $('bt-end')) $('bt-end').value = patch.end;
        if (patch.capital != null && $('bt-capital')) $('bt-capital').value = patch.capital;
        if (patch.params != null && $('bt-params')) $('bt-params').value = patch.params;
        if (patch.goal && $('ai-goal')) $('ai-goal').value = patch.goal;
        if (typeof window.saveSettings === 'function') {
            // not exported — trigger input event
            ['bt-params', 'bt-capital', 'bt-start', 'bt-end'].forEach(function (id) {
                var el = $(id);
                if (el) el.dispatchEvent(new Event('input'));
            });
        }
        return workspace();
    }

    function compactBacktest(r) {
        if (!r) return r;
        return {
            status: r.status,
            strategy: r.strategy,
            data_file: r.data_file,
            params: r.params,
            candles: r.candles,
            total_return_pct: r.total_return_pct,
            sharpe_ratio: r.sharpe_ratio,
            max_drawdown_pct: r.max_drawdown_pct,
            total_trades: r.total_trades,
            win_rate_pct: r.win_rate_pct,
            profit_factor: r.profit_factor,
            final_capital: r.final_capital,
            initial_capital: r.initial_capital
        };
    }

    function showResultPanel(data) {
        if (!data) return;
        var box = $('results-content');
        var empty = $('results-empty');
        var pill = $('result-status-pill');
        if (empty) empty.style.display = 'none';
        if (!box) return;
        box.style.display = 'block';

        var ret = Number(data.total_return_pct || 0);
        var up = ret >= 0;
        var strat = data.strategy || '—';
        var file = data.data_file || '';
        var params = data.params || '';
        var retTxt = (up ? '+' : '') + ret.toFixed(2) + '%';
        var sharpe = Number(data.sharpe_ratio || 0).toFixed(2);
        var dd = Number(data.max_drawdown_pct || 0).toFixed(2) + '%';
        var trades = String(data.total_trades != null ? data.total_trades : '—');
        var finalEq = data.final_capital != null
            ? ('$' + Number(data.final_capital).toFixed(2))
            : '—';

        if (pill) {
            pill.textContent = up ? 'PROFIT' : 'LOSS';
            pill.style.background = up ? 'var(--success-bg)' : 'var(--danger-bg)';
            pill.style.color = up ? 'var(--success)' : 'var(--danger)';
            pill.style.border = 'none';
        }

        box.innerHTML =
            '<div class="rail-result-card">'
            +  '<div class="rr-top">'
            +    '<div>'
            +      '<div class="rr-title">' + escHtml(strat) + '</div>'
            +      (file ? '<div class="rr-sub">' + escHtml(file) + '</div>' : '')
            +    '</div>'
            +    '<span class="rr-badge ' + (up ? 'up' : 'down') + '">' + (up ? '盈利' : '亏损') + '</span>'
            +  '</div>'
            +  '<div class="rr-grid">'
            +    '<div class="rr-cell"><div class="rr-val ' + (up ? 'positive' : 'negative') + '">' + escHtml(retTxt) + '</div><div class="rr-lab">收益</div></div>'
            +    '<div class="rr-cell"><div class="rr-val">' + escHtml(sharpe) + '</div><div class="rr-lab">夏普</div></div>'
            +    '<div class="rr-cell"><div class="rr-val negative">' + escHtml(dd) + '</div><div class="rr-lab">最大回撤</div></div>'
            +    '<div class="rr-cell"><div class="rr-val">' + escHtml(trades) + '</div><div class="rr-lab">成交</div></div>'
            +  '</div>'
            +  (params ? '<div class="rr-params">' + escHtml(params) + '</div>' : '')
            +  (finalEq !== '—' ? '<div class="rr-params" style="border-top:none;padding-top:0;color:var(--text-muted);">期末净值 ' + escHtml(finalEq)
                + (data.candles != null ? ' · ' + data.candles + ' bars' : '') + '</div>' : '')
            + '</div>';
    }

    async function toolListDatasets() {
        var r = await fetchWithTimeout('/api/backtest/datasets', {}, 15000, '数据集读取');
        var j = await r.json();
        return j;
    }

    async function toolRunBacktest(args) {
        var ws = workspace();
        var body = {
            strategy: args.strategy || ws.strategy,
            data_file: args.data_file || ws.data_file,
            start: args.start || ws.start,
            end: args.end || ws.end,
            capital: args.capital != null ? args.capital : ws.capital,
            params: args.params != null ? args.params : ws.params
        };
        var r = await fetchWithTimeout('/api/backtest', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body)
        }, 120000, '回测');
        var j = await r.json();
        if (j && j.status !== 'error') {
            lastVerifiedBacktest = compactBacktest(j);
            showResultPanel(j);
            if (args.params && $('bt-params')) $('bt-params').value = args.params;
        }
        return compactBacktest(j);
    }

    async function toolSweep(args) {
        var ws = workspace();
        var body = {
            strategy: args.strategy || ws.strategy,
            data_file: args.data_file || ws.data_file,
            start: args.start || ws.start,
            end: args.end || ws.end,
            capital: args.capital != null ? args.capital : ws.capital,
            goal: args.goal || ws.goal,
            mode: args.mode || 'quick',
            params: args.params != null ? args.params : ws.params
        };
        var r = await fetchWithTimeout('/api/backtest/optimize', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(body)
        }, body.mode === 'full' ? 240000 : 120000, '参数扫描');
        var j = await r.json();
        if (j && j.optimized) {
            lastVerifiedBacktest = compactBacktest(j.optimized);
            showResultPanel(j.optimized);
        }
        if (j && j.optimized_params && $('bt-params')) {
            $('bt-params').value = j.optimized_params;
        }
        // shrink payload
        return {
            status: j.status,
            message: j.message,
            tested: j.tested,
            profitable: j.profitable,
            with_trades: j.with_trades,
            optimized_params: j.optimized_params,
            optimized: compactBacktest(j.optimized),
            leaderboard: (j.leaderboard || []).slice(0, 10)
        };
    }

    async function toolCompare(args) {
        var ws = workspace();
        var capital = args.capital != null ? args.capital : ws.capital;
        var requested = Array.isArray(args.strategies) && args.strategies.length
            ? args.strategies : ['grid', 'trend'];
        var strategies = requested.filter(function (name, index) {
            return ['grid', 'dca', 'trend'].indexOf(name) >= 0 && requested.indexOf(name) === index;
        });
        var goal = args.goal || 'balanced';
        var comparison = {};
        for (var i = 0; i < strategies.length; i++) {
            if (abortFlag) throw new Error('strategy comparison stopped');
            appendText('system', '策略对比 ' + (i + 1) + '/' + strategies.length + ' · 正在扫描 ' + strategies[i] + '…', 'step-ai');
            var sweep = await toolSweep({
                strategy: strategies[i], data_file: args.data_file || ws.data_file,
                start: args.start || ws.start, end: args.end || ws.end,
                capital: capital, goal: goal, mode: 'quick', params: ''
            });
            comparison[strategies[i]] = sweep.optimized || { status: 'error', message: sweep.message };
        }
        var ranked = strategies.map(function (strategy) {
            var metrics = comparison[strategy] || {};
            var score = scoreStrategyCandidate({ optimized: metrics }, goal, 'balanced');
            return { strategy: strategy, metrics: metrics, eligible: score.eligible, score: score.score, reason: score.reason };
        }).sort(function (a, b) { return b.score - a.score; });
        return {
            status: 'ok', strategies: strategies, goal: goal,
            comparison: comparison,
            recommended: ranked.find(function (row) { return row.eligible; }) || null,
            ranked: ranked,
            live_applied: false
        };
    }

    function scoreStrategyCandidate(result, goal, risk) {
        var r = result && result.optimized ? result.optimized : null;
        if (!r) return { eligible: false, score: -Infinity, reason: '没有有效回测结果' };
        var ret = Number(r.total_return_pct);
        var sharpe = Number(r.sharpe_ratio);
        var dd = Math.abs(Number(r.max_drawdown_pct));
        var trades = Number(r.total_trades);
        var maxDd = risk === 'conservative' ? 5 : risk === 'aggressive' ? 15 : 10;
        if (![ret, sharpe, dd, trades].every(Number.isFinite)) {
            return { eligible: false, score: -Infinity, reason: '指标不完整' };
        }
        if (trades < 3) return { eligible: false, score: -Infinity, reason: '成交不足 3 笔' };
        if (ret <= 0) return { eligible: false, score: -Infinity, reason: '验证收益不为正' };
        if (dd > maxDd) return { eligible: false, score: -Infinity, reason: '回撤超过 ' + maxDd + '%' };
        var score;
        if (goal === 'return') score = ret - dd * 0.35;
        else if (goal === 'drawdown') score = ret * 0.25 + sharpe - dd;
        else if (goal === 'sharpe') score = sharpe * 5 + ret * 0.15 - dd * 0.4;
        else score = ret + sharpe * 2 - dd * 0.75;
        // Tiny-sample strategies are allowed above the hard floor but rank lower.
        score -= trades < 8 ? (8 - trades) * 0.2 : 0;
        return { eligible: true, score: score, max_drawdown_budget_pct: maxDd };
    }

    function missionStrategies(universe) {
        if (universe === 'grid' || universe === 'trend') return [universe];
        return universe === 'all' ? ['grid', 'trend', 'dca'] : ['grid', 'trend'];
    }

    function setMissionStage(stage) {
        var order = ['context', 'screen', 'backtest', 'rank', 'ai', 'recommend'];
        var active = order.indexOf(stage);
        document.querySelectorAll('#mission-stages [data-stage]').forEach(function (el) {
            var idx = order.indexOf(el.getAttribute('data-stage'));
            el.classList.toggle('done', active >= 0 && idx < active);
            el.classList.toggle('active', idx === active);
        });
    }

    function renderStrategyCandidates(research) {
        var box = $('strategy-candidates');
        if (!box) return;
        var candidates = research.candidates || [];
        box.style.display = 'flex';
        box.innerHTML = candidates.map(function (candidate, index) {
            var r = candidate.result.optimized || {};
            var verdict = candidate.eligible ? ('评分 ' + candidate.score.toFixed(2)) : candidate.reason;
            return '<button type="button" class="strategy-candidate ' + (index === 0 && candidate.eligible ? 'recommended' : '')
                + '" data-candidate-index="' + index + '"><div class="candidate-name"><span>'
                + escHtml(candidate.strategy) + '</span><span class="candidate-score">' + escHtml(verdict) + '</span></div>'
                + '<div class="candidate-metrics">收益 ' + Number(r.total_return_pct || 0).toFixed(2) + '% · Sharpe '
                + Number(r.sharpe_ratio || 0).toFixed(2) + '<br>DD ' + Math.abs(Number(r.max_drawdown_pct || 0)).toFixed(2)
                + '% · ' + Number(r.total_trades || 0) + ' 笔</div></button>';
        }).join('');
        box.querySelectorAll('[data-candidate-index]').forEach(function (button) {
            button.addEventListener('click', function () {
                var candidate = candidates[Number(button.getAttribute('data-candidate-index'))];
                if (!candidate || !candidate.result.optimized) return;
                applyWorkspace({ strategy: candidate.strategy, params: candidate.result.optimized_params });
                lastVerifiedBacktest = candidate.result.optimized;
                showResultPanel(candidate.result.optimized);
                appendText('system', '已把 ' + candidate.strategy + ' 候选加载到工作区；尚未应用到实盘。', 'step-ok');
            });
        });
    }

    async function toolResearchStrategies(args) {
        args = args || {};
        var goal = args.goal || val('mission-goal', 'balanced');
        var risk = args.risk || val('mission-risk', 'balanced');
        var universe = args.universe || val('mission-universe', 'core');
        var ws = workspace();
        setMissionStage('context');
        var liveParts = await Promise.all([
            fetch('/api/status').then(function (r) { return r.json(); }),
            fetch('/api/positions').then(function (r) { return r.json(); }),
            fetch('/api/agent/status').then(function (r) { return r.json(); })
        ]);
        var context = {
            equity: Number(liveParts[0].equity || 0),
            trading_paused: !!(liveParts[2].policy && liveParts[2].policy.trading_paused),
            open_positions: Array.isArray(liveParts[1].positions) ? liveParts[1].positions.length : 0,
            agent_gate: liveParts[2].status || 'unknown'
        };
        setMissionStage('screen');
        var strategies = missionStrategies(universe);
        var candidates = [];
        setMissionStage('backtest');
        for (var i = 0; i < strategies.length; i++) {
            if (abortFlag) throw new Error('strategy mission stopped');
            appendText('system', '研究回测 ' + (i + 1) + '/' + strategies.length + ' · ' + strategies[i], 'step-ai');
            var result = await toolSweep({
                strategy: strategies[i], data_file: ws.data_file, start: ws.start, end: ws.end,
                capital: ws.capital, goal: goal, mode: 'quick', params: ''
            });
            var ranked = scoreStrategyCandidate(result, goal, risk);
            if (strategies[i] === 'dca' && ranked.eligible) {
                ranked = { eligible: false, score: ranked.score, reason: 'DCA 当前仅研究，尚未开放实盘审批' };
            }
            candidates.push(Object.assign({ strategy: strategies[i], result: result }, ranked));
        }
        setMissionStage('rank');
        candidates.sort(function (a, b) {
            if (a.eligible !== b.eligible) return a.eligible ? -1 : 1;
            return b.score - a.score;
        });
        var recommended = candidates.find(function (candidate) { return candidate.eligible; }) || null;
        var research = {
            status: recommended ? 'ok' : 'no_candidate',
            goal: goal,
            risk: risk,
            universe: universe,
            context: context,
            candidates: candidates,
            recommended: recommended ? {
                strategy: recommended.strategy,
                params: recommended.result.optimized_params,
                metrics: recommended.result.optimized,
                score: recommended.score
            } : null,
            next_action: recommended ? 'review_and_load_candidate' : 'broaden_data_or_risk_budget',
            live_applied: false
        };
        renderStrategyCandidates(research);
        setMissionStage('recommend');
        return research;
    }

    function compactResearchEvidence(research) {
        return {
            goal: research.goal,
            risk: research.risk,
            universe: research.universe,
            candidates: (research.candidates || []).map(function (candidate) {
                var metrics = candidate.result && candidate.result.optimized;
                return {
                    strategy: candidate.strategy,
                    params: candidate.result && candidate.result.optimized_params,
                    eligible: candidate.eligible,
                    reason: candidate.reason,
                    metrics: compactBacktest(metrics)
                };
            }).slice(-12)
        };
    }

    async function runAdaptiveAiResearch(research, args) {
        var cfg = getProvider();
        if (!cfg.url || !cfg.model || (!cfg.key && cfg.provider !== 'ollama')) {
            research.adaptive_status = 'not_configured';
            research.adaptive_rounds = 0;
            return research;
        }
        setMissionStage('ai');
        var catalog = await toolListDatasets();
        var allowedDatasets = {};
        (catalog.datasets || []).forEach(function (dataset) {
            if (dataset.file && dataset.start && dataset.end) {
                allowedDatasets[dataset.file] = {
                    start: dataset.start,
                    end: dataset.end,
                    candles: dataset.candles
                };
            }
        });
        if (!Object.keys(allowedDatasets).length) {
            research.adaptive_status = 'no_datasets';
            research.adaptive_rounds = 0;
            return research;
        }

        var seen = {};
        var experimentEvidence = [];
        research.adaptive_status = 'running';
        for (var round = 1; round <= MAX_AI_RESEARCH_ROUNDS; round++) {
            if (abortFlag) throw new Error('adaptive strategy research stopped');
            appendText('system', 'AI 策略迭代 ' + round + '/' + MAX_AI_RESEARCH_ROUNDS
                + ' · 正在基于真实回测结果提出下一组实验…', 'step-ai');
            var prompt = {
                objective: { goal: args.goal, risk: args.risk, universe: args.universe },
                allowed_datasets: allowedDatasets,
                verified_baseline: compactResearchEvidence(research),
                prior_ai_experiments: experimentEvidence.slice(-9)
            };
            var reply;
            try {
                reply = await callModel([
                    { role: 'system', content: 'You are a bounded quant research planner. Propose hypotheses for REAL backtests only. You may select only grid, trend, or dca and only datasets and date ranges in the supplied catalog. Never request code, paths, capital changes, credentials, live trading, or strategy deployment. Return JSON only in this exact shape: {"hypothesis":"short reason","experiments":[{"strategy":"grid|trend|dca","data_file":"exact catalog filename","start":"YYYY-MM-DD","end":"YYYY-MM-DD","params":"key=value,key=value"}]}. Return at most 3 experiments. Vary strategy, market window, and valid numeric parameters based on prior verified failures.' },
                    { role: 'user', content: JSON.stringify(prompt) }
                ], { chatOnly: true });
            } catch (modelError) {
                research.adaptive_status = 'model_error';
                research.adaptive_error = String(modelError.message || modelError).slice(0, 300);
                appendText('system', 'AI 研究模型调用失败：' + research.adaptive_error, 'step-warn');
                break;
            }
            var plan = window.QuantAgentProtocol.extractJsonObject(reply.content);
            var validated = window.QuantAgentProtocol.validateResearchExperiments(plan, {
                allowedDatasets: allowedDatasets,
                maxExperiments: MAX_AI_EXPERIMENTS_PER_ROUND
            });
            if (validated.rejected.length) {
                appendText('system', 'AI 方案安全校验：拒绝 ' + validated.rejected.length + ' 个越界实验。', 'step-warn');
            }
            if (!validated.experiments.length) {
                experimentEvidence.push({ round: round, status: 'invalid_plan' });
                appendText('system', 'AI 第 ' + round + ' 轮未给出可执行的合规实验，继续请求修正。', 'step-warn');
                research.adaptive_rounds = round;
                continue;
            }

            for (var i = 0; i < validated.experiments.length; i++) {
                var experiment = validated.experiments[i];
                var signature = JSON.stringify(experiment);
                if (seen[signature]) continue;
                seen[signature] = true;
                appendText('system', 'AI 实验 ' + round + '.' + (i + 1) + ' · '
                    + experiment.strategy + ' · 正在真实回测', 'step-ai');
                var metrics = await toolRunBacktest({
                    strategy: experiment.strategy,
                    data_file: experiment.data_file,
                    start: experiment.start,
                    end: experiment.end,
                    capital: workspace().capital,
                    params: experiment.params
                });
                var result = { optimized: metrics, optimized_params: experiment.params };
                var ranked = scoreStrategyCandidate(result, args.goal, args.risk);
                if (experiment.strategy === 'dca' && ranked.eligible) {
                    ranked = { eligible: false, score: ranked.score, reason: 'DCA 当前仅研究，尚未开放实盘审批' };
                }
                var candidate = Object.assign({
                    strategy: experiment.strategy,
                    result: result,
                    source: 'ai',
                    hypothesis: validated.hypothesis,
                    round: round,
                    data_file: experiment.data_file
                }, ranked);
                research.candidates.push(candidate);
                experimentEvidence.push({
                    round: round,
                    experiment: experiment,
                    eligible: candidate.eligible,
                    reason: candidate.reason,
                    metrics: compactBacktest(metrics)
                });
            }
            research.candidates.sort(function (a, b) {
                if (a.eligible !== b.eligible) return a.eligible ? -1 : 1;
                return b.score - a.score;
            });
            var recommended = research.candidates.find(function (candidate) { return candidate.eligible; });
            research.adaptive_rounds = round;
            renderStrategyCandidates(research);
            if (recommended) {
                research.status = 'ok';
                research.adaptive_status = 'candidate_found';
                research.recommended = {
                    strategy: recommended.strategy,
                    params: recommended.result.optimized_params,
                    metrics: recommended.result.optimized,
                    score: recommended.score,
                    source: 'ai',
                    hypothesis: recommended.hypothesis
                };
                research.next_action = 'review_and_load_candidate';
                return research;
            }
        }
        if (research.adaptive_status === 'running') research.adaptive_status = 'exhausted';
        research.status = 'no_candidate';
        research.next_action = 'review_ai_experiments';
        renderStrategyCandidates(research);
        return research;
    }

    async function startStrategyMission() {
        if (agentBusy) return;
        abortFlag = false;
        setBusy(true);
        var args = {
            goal: val('mission-goal', 'balanced'),
            risk: val('mission-risk', 'balanced'),
            universe: val('mission-universe', 'core')
        };
        appendText('user', '启动策略研究 · 目标 ' + args.goal + ' · 风险 ' + args.risk + ' · 范围 ' + args.universe);
        try {
            var research = await toolResearchStrategies(args);
            if (!research.recommended) research = await runAdaptiveAiResearch(research, args);
            setMissionStage('recommend');
            appendTool('research_strategies', args, research, window.QuantAgentProtocol.classifyToolOutcome(research));
            if (!research.recommended) {
                var rounds = Number(research.adaptive_rounds || 0);
                var reason = research.adaptive_status === 'not_configured'
                    ? 'AI 模型尚未配置，因此只完成了固定策略扫参。'
                    : research.adaptive_status === 'model_error'
                        ? 'AI 模型调用失败，已保留固定扫参与错误信息。'
                        : '固定扫参及 ' + rounds + ' 轮 AI 假设实验均未找到合格候选。';
                appendText('assistant', reason + ' 所有展示指标都来自真实回测；本轮不会上线任何策略。');
            } else {
                var best = research.recommended;
                appendText('assistant', '推荐 ' + best.strategy + '：' + best.params
                    + '\n已通过本轮真实扫参和风险预算筛选。点击上方候选卡可加载到工作区；尚未应用实盘。');
                var cfg = getProvider();
                if (cfg.url && cfg.model && cfg.key) {
                    var evidence = JSON.stringify({ goal: research.goal, risk: research.risk, context: research.context, recommended: best, candidates: research.candidates.map(function (c) {
                        return { strategy: c.strategy, eligible: c.eligible, score: c.score, reason: c.reason, metrics: c.result.optimized };
                    }) });
                    var reply = await callModel([
                        { role: 'system', content: systemPrompt() + '\nExplain verified strategy research evidence. Do not invent metrics and do not call or suggest automatic live execution.' },
                        { role: 'user', content: '[verified_strategy_research] ' + evidence + '\n请用中文给出简短策略判断、适用行情、主要风险和下一步验证建议。' }
                    ], { chatOnly: true });
                    appendText('assistant', reply.content || '', null, 'AI 策略解读 · ' + (reply.model || cfg.model));
                }
            }
        } catch (e) {
            appendText('system', '策略研究失败: ' + e.message, 'step-err');
        }
        setBusy(false);
        refreshAgentGovernance();
    }

    /* 人工确认闸门。
       原来这里只检查 args.confirm === true —— 而 confirm 是**模型自己填的参数**，
       等于让被约束的一方给自己签字：模型写个 confirm:true 就能直接改实盘策略。
       现在模型只能"请求"，真正落到 /api/strategy 必须由人点一次按钮。 */
    var pendingLive = null;
    function requestLiveApproval(proposal, rawParams) {
        return new Promise(function (resolve) {
            pendingLive = { proposal: proposal, resolve: resolve };
            renderLiveApproval(proposal, rawParams);
        });
    }
    function renderLiveApproval(proposal, rawParams) {
        var chat = $('agent-chat'); if (!chat) return;
        var zh = (localStorage.getItem('lighter-lang') || 'cn') === 'cn';
        var strategyName = proposal.input.strategy;
        var policyText = (proposal.decision.checks || []).join(' · ');
        var box = document.createElement('div');
        box.className = 'agent-approval';
        box.innerHTML =
            '<div class="approval-head">' + (zh ? '需要你确认：写入实盘' : 'Approval required: write to LIVE') + '</div>' +
            '<div class="approval-body">' +
              '<div><b>' + escHtml(strategyName) + '</b></div>' +
              '<div class="approval-params">' + escHtml(rawParams) + '</div>' +
              '<div class="approval-warn">' + escHtml(policyText) + '</div>' +
              '<div class="approval-warn">' + (zh
                 ? '后端风控已通过。点击后仍会按当前账户状态重新检查，再写入策略。'
                 : 'Server policy passed. Current account state is checked again before apply.') + '</div>' +
            '</div>' +
            '<div class="approval-actions">' +
              '<button class="btn btn-ai" data-act="deny">' + (zh ? '拒绝' : 'Deny') + '</button>' +
              '<button class="btn btn-primary" data-act="allow">' + (zh ? '确认写入实盘' : 'Apply to live') + '</button>' +
            '</div>';
        box.addEventListener('click', function (e) {
            var act = e.target && e.target.getAttribute && e.target.getAttribute('data-act');
            if (!act || !pendingLive) return;
            var p = pendingLive; pendingLive = null;
            box.querySelectorAll('button').forEach(function (b) { b.disabled = true; });
            box.classList.add(act === 'allow' ? 'approved' : 'denied');
            p.resolve(act === 'allow');
        });
        chat.appendChild(box);
        chat.scrollTop = chat.scrollHeight;
    }

    async function toolApplyLive(args) {
        if (!args || args.confirm !== true) {
            return { status: 'error', message: 'confirm must be true; user must explicitly request live apply' };
        }
        var strategy = args.strategy || workspace().strategy;
        var paramsStr = args.params || workspace().params;
        var params = {};
        String(paramsStr).split(',').forEach(function (pair) {
            var kv = pair.split('=');
            if (kv.length === 2) {
                var v = parseFloat(kv[1].trim());
                params[kv[0].trim()] = isNaN(v) ? kv[1].trim() : v;
            }
        });
        if (!lastVerifiedBacktest || lastVerifiedBacktest.strategy !== strategy) {
            return { status: 'error', message: 'run a verified backtest for this strategy before proposing live apply' };
        }
        var proposalResponse = await fetch('/api/agent/proposals', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                strategy: strategy,
                params: params,
                evidence: {
                    data_file: lastVerifiedBacktest.data_file || workspace().data_file,
                    start: workspace().start,
                    end: workspace().end,
                    capital: workspace().capital,
                    total_return_pct: Number(lastVerifiedBacktest.total_return_pct),
                    sharpe_ratio: Number(lastVerifiedBacktest.sharpe_ratio),
                    max_drawdown_pct: Number(lastVerifiedBacktest.max_drawdown_pct),
                    total_trades: Number(lastVerifiedBacktest.total_trades)
                },
                rationale: String(args.rationale || 'model-proposed after verified backtest')
            })
        }).then(function (r) { return r.json(); });
        var proposal = proposalResponse.proposal;
        if (!proposal || !proposal.decision || !proposal.decision.allowed) {
            return {
                status: 'error',
                message: 'backend policy rejected proposal',
                violations: proposal && proposal.decision ? proposal.decision.violations : []
            };
        }

        // 模型只能创建提案；真正放行必须由人在服务端签发的同一提案上点击确认。
        var approved = await requestLiveApproval(proposal, String(paramsStr));
        if (!approved) {
            return { status: 'error', message: 'denied by user; live config unchanged' };
        }
        var r = await fetch('/api/agent/proposals/' + encodeURIComponent(proposal.id) + '/apply', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ approval_phrase: proposal.approval_phrase })
        });
        var result = await r.json();
        refreshAgentGovernance();
        return result;
    }

    async function refreshAgentGovernance() {
        try {
            var pair = await Promise.all([
                fetch('/api/agent/status').then(function (r) { return r.json(); }),
                fetch('/api/agent/audit').then(function (r) { return r.json(); })
            ]);
            var status = pair[0];
            var audit = pair[1];
            var statusEl = $('agent-policy-status');
            if (statusEl) {
                statusEl.textContent = status.status === 'ready'
                    ? 'READY · 提案模式 · 人工批准'
                    : 'BLOCKED · ' + (status.policy && status.policy.emergency_triggered ? '紧急风控' : '交易暂停');
                statusEl.className = 'rail-result-empty ' + (status.status === 'ready' ? 'positive' : 'negative');
            }
            var list = $('agent-audit-list');
            if (list) {
                var records = (audit.records || []).slice(0, 5);
                list.innerHTML = records.length ? records.map(function (p) {
                    return '<div class="approval-params">' + escHtml(p.created_at.slice(11, 19) + ' · ' + p.input.strategy + ' · ' + p.status) + '</div>';
                }).join('') : '<div class="rail-result-empty">暂无提案</div>';
            }
        } catch (e) {
            var el = $('agent-policy-status');
            if (el) el.textContent = '策略状态读取失败';
        }
    }

    async function executeTool(name, args) {
        args = args || {};
        try {
            var result;
            if (name === 'list_datasets') result = await toolListDatasets();
            else if (name === 'get_workspace') result = workspace();
            else if (name === 'set_workspace') result = applyWorkspace(args);
            else if (name === 'run_backtest') result = await toolRunBacktest(args);
            else if (name === 'run_param_sweep') result = await toolSweep(args);
            else if (name === 'compare_strategies') result = await toolCompare(args);
            else if (name === 'research_strategies') result = await toolResearchStrategies(args);
            else if (name === 'apply_to_live') result = await toolApplyLive(args);
            else result = { status: 'error', message: 'Unknown tool: ' + name };
            var outcome = window.QuantAgentProtocol.classifyToolOutcome(result);
            var ok = outcome !== 'error';
            appendTool(name, args, result, outcome);
            return { ok: ok, outcome: outcome, result: result };
        } catch (e) {
            appendTool(name, args, { error: e.message }, false);
            return { ok: false, result: { status: 'error', message: e.message } };
        }
    }

    function getProvider() {
        var ctx = parseInt(val('ai-context-window', String(DEFAULT_CONTEXT_WINDOW)), 10);
        if (!Number.isFinite(ctx) || ctx < 32000) ctx = DEFAULT_CONTEXT_WINDOW;
        if (ctx > 2000000) ctx = 2000000;
        // 单次输出上限：允许到 131072（部分长上下文模型的 completion 上限）
        var out = parseInt(val('ai-max-tokens', '32768'), 10);
        if (!Number.isFinite(out) || out < 256) out = 32768;
        if (out > 131072) out = 131072;
        // 输出不应吃掉整个上下文
        out = Math.min(out, Math.floor(ctx * 0.25));
        return {
            url: val('ai-url', ''),
            model: val('ai-model', ''),
            key: val('ai-key', ''),
            provider: val('ai-provider', 'openai'),
            maxTokens: out,
            contextWindow: ctx
        };
    }

    /** 粗估 token：中英混合约 1 token ≈ 2–3 字符；工具 JSON 偏密用 /3 */
    function estimateTextTokens(text) {
        var s = String(text || '');
        if (!s) return 0;
        var cjk = (s.match(/[\u3400-\u9FFF]/g) || []).length;
        var rest = s.length - cjk;
        return Math.ceil(cjk / 1.5 + rest / 3.5);
    }

    function estimateMessagesTokens(messages) {
        var total = 0;
        (messages || []).forEach(function (m) {
            total += 8; // role / framing
            if (typeof m.content === 'string') total += estimateTextTokens(m.content);
            else if (m.content != null) total += estimateTextTokens(JSON.stringify(m.content));
            if (m.tool_calls) total += estimateTextTokens(JSON.stringify(m.tool_calls));
            if (m.name) total += estimateTextTokens(m.name);
        });
        return total;
    }

    function updateContextMeter() {
        var cfg = getProvider();
        var est = estimateMessagesTokens(history);
        lastEstTokens = est;
        var ratio = cfg.contextWindow > 0 ? est / cfg.contextWindow : 0;
        var fill = $('agent-ctx-fill');
        var text = $('agent-ctx-text');
        if (fill) {
            fill.style.width = Math.min(100, Math.round(ratio * 1000) / 10) + '%';
            fill.classList.remove('warn', 'hot');
            if (ratio >= 0.9) fill.classList.add('hot');
            else if (ratio >= 0.7) fill.classList.add('warn');
        }
        if (text) {
            text.textContent = est.toLocaleString() + ' / ' + cfg.contextWindow.toLocaleString()
                + ' (' + Math.round(ratio * 100) + '%)';
        }
        return { est: est, window: cfg.contextWindow, ratio: ratio };
    }

    function messagePreview(m, maxLen) {
        maxLen = maxLen || 400;
        var role = m.role || '?';
        var body = '';
        if (typeof m.content === 'string') body = m.content;
        else if (m.tool_calls) body = '[tool_calls] ' + JSON.stringify(m.tool_calls);
        else if (m.content != null) body = JSON.stringify(m.content);
        body = body.replace(/\s+/g, ' ').trim();
        if (body.length > maxLen) body = body.slice(0, maxLen) + '…';
        return role + ': ' + body;
    }

    /**
     * Compact：把 system 之后、最近 N 条之前的历史压成一条摘要消息。
     * force=true 时忽略阈值强制压缩。
     */
    async function compactHistory(force) {
        var cfg = getProvider();
        var meter = updateContextMeter();
        if (!force && meter.ratio < COMPACT_TRIGGER_RATIO) {
            return { did: false, reason: 'below_threshold', meter: meter };
        }
        if (history.length < COMPACT_KEEP_RECENT + 3) {
            return { did: false, reason: 'too_short', meter: meter };
        }

        var sys = null;
        var rest = history.slice();
        if (rest[0] && rest[0].role === 'system') {
            sys = rest[0];
            rest = rest.slice(1);
        }
        if (rest.length <= COMPACT_KEEP_RECENT) {
            return { did: false, reason: 'nothing_to_drop', meter: meter };
        }

        var drop = rest.slice(0, rest.length - COMPACT_KEEP_RECENT);
        var keep = rest.slice(rest.length - COMPACT_KEEP_RECENT);
        var transcript = drop.map(function (m) { return messagePreview(m, 500); }).join('\n');

        var summary = '';
        // 优先用模型做 compact；失败则本地截断摘要
        try {
            if (cfg.url && cfg.model && (cfg.key || cfg.provider === 'ollama')) {
                var compactMsgs = [
                    {
                        role: 'system',
                        content: 'You compress chat history for a quant research agent. '
                            + 'Output a dense bullet summary in Chinese: goals, datasets, strategies tried, '
                            + 'key backtest numbers, best params, open questions. No fluff. Keep under ~1200 tokens.'
                    },
                    {
                        role: 'user',
                        content: 'Compress the following history:\n\n' + transcript.slice(0, 120000)
                    }
                ];
                var reply = await callModel(compactMsgs, { chatOnly: true });
                summary = (reply && reply.content || '').trim();
            }
        } catch (e) {
            summary = '';
        }
        if (!summary) {
            summary = '【本地 compact】保留要点摘录：\n' + transcript.slice(0, 6000);
        }

        var compacted = [];
        if (sys) compacted.push(sys);
        compacted.push({
            role: 'user',
            content: '[COMPACTED HISTORY · ' + drop.length + ' messages folded]\n' + summary
        });
        compacted.push({
            role: 'assistant',
            content: '已记住压缩摘要。后续在此基础上继续；需要细节可再跑工具核实。'
        });
        history = compacted.concat(keep);

        var after = updateContextMeter();
        appendText(
            'system',
            'Compact 完成：折叠 ' + drop.length + ' 条 → 估算 '
                + meter.est.toLocaleString() + ' → ' + after.est.toLocaleString()
                + ' / ' + after.window.toLocaleString(),
            'step-ok'
        );
        return { did: true, dropped: drop.length, before: meter.est, after: after.est, meter: after };
    }

    async function ensureContextBudget() {
        var meter = updateContextMeter();
        if (meter.ratio >= COMPACT_TRIGGER_RATIO) {
            appendText('system', '上下文占用 ' + Math.round(meter.ratio * 100) + '%，自动 Compact…', 'step-warn');
            await compactHistory(true);
        }
    }

    async function callModel(messages, opts) {
        var cfg = getProvider();
        if (!cfg.url) throw new Error('请先填写 API 地址');
        if (!cfg.model) throw new Error('请先填写模型 ID');
        if (!cfg.key && cfg.provider !== 'ollama') throw new Error('请先填写 API Key');

        var isAnthropic = cfg.provider === 'claude' || cfg.url.indexOf('anthropic.com') >= 0;
        if (isAnthropic) {
            return callAnthropic(cfg, messages, opts);
        }
        return callOpenAICompat(cfg, messages, opts);
    }

    async function fetchWithTimeout(url, options, timeoutMs, label) {
        var controller = new AbortController();
        activeRequestController = controller;
        options = Object.assign({}, options || {}, { signal: controller.signal });
        var timedOut = false;
        var timer = setTimeout(function () { timedOut = true; controller.abort(); }, timeoutMs);
        try {
            return await fetch(url, options);
        } catch (e) {
            if (e && e.name === 'AbortError') {
                throw new Error(timedOut
                    ? (label || '请求') + '超时（' + Math.round(timeoutMs / 1000) + ' 秒）'
                    : (label || '请求') + '已停止');
            }
            throw e;
        } finally {
            clearTimeout(timer);
            if (activeRequestController === controller) activeRequestController = null;
        }
    }

    async function callOpenAICompat(cfg, messages, opts) {
        opts = opts || {};
        // max_tokens = 本轮允许的最大「输出」token，不是累计账单
        var body = {
            model: cfg.model,
            messages: messages,
            max_tokens: cfg.maxTokens,
            temperature: opts.chatOnly ? 0.3 : 0.2
        };
        // 闲聊不带 tools，避免模型硬走工具/长自我介绍
        if (!opts.chatOnly) {
            body.tools = TOOLS;
            body.tool_choice = 'auto';
        }
        var headers = { 'Content-Type': 'application/json' };
        if (cfg.key) headers.Authorization = 'Bearer ' + cfg.key;

        var r = await fetchWithTimeout(cfg.url, { method: 'POST', headers: headers, body: JSON.stringify(body) }, 45000, '模型请求');
        var text = await r.text();
        if (!r.ok) {
            // Fallback without tools if provider rejects tools schema
            if (!opts.chatOnly && r.status === 400 && /tool/i.test(text)) {
                return callOpenAITextFallback(cfg, messages);
            }
            throw new Error('HTTP ' + r.status + ': ' + text.slice(0, 400));
        }
        var d = JSON.parse(text);
        var msg = d.choices && d.choices[0] && d.choices[0].message;
        if (!msg) throw new Error('Unexpected AI response: ' + text.slice(0, 300));
        var out = normalizeOpenAIMessage(msg);
        out.usage = d.usage || null;
        out.model = d.model || cfg.model;
        return out;
    }

    async function callOpenAITextFallback(cfg, messages) {
        var extra = {
            role: 'system',
            content: 'Tools are not available via function calling. When you need a tool, output ONLY one line:\n'
                + 'TOOL_CALL {"name":"tool_name","arguments":{...}}\n'
                + 'When finished researching, output:\nFINAL your summary'
        };
        var body = {
            model: cfg.model,
            messages: [extra].concat(messages),
            max_tokens: cfg.maxTokens,
            temperature: 0.2
        };
        var headers = { 'Content-Type': 'application/json' };
        if (cfg.key) headers.Authorization = 'Bearer ' + cfg.key;
        var r = await fetchWithTimeout(cfg.url, { method: 'POST', headers: headers, body: JSON.stringify(body) }, 45000, '模型请求');
        var text = await r.text();
        if (!r.ok) throw new Error('HTTP ' + r.status + ': ' + text.slice(0, 400));
        var d = JSON.parse(text);
        var content = d.choices && d.choices[0] && d.choices[0].message && d.choices[0].message.content || '';
        var out = parseTextToolProtocol(content);
        out.usage = d.usage || null;
        out.model = d.model || cfg.model;
        return out;
    }

    async function callAnthropic(cfg, messages, opts) {
        opts = opts || {};
        // Convert OpenAI messages + tools to Anthropic messages API
        var sys = '';
        var anthMsgs = [];
        messages.forEach(function (m) {
            if (m.role === 'system') sys += (sys ? '\n' : '') + m.content;
            else if (m.role === 'tool') {
                anthMsgs.push({
                    role: 'user',
                    content: [{ type: 'tool_result', tool_use_id: m.tool_call_id, content: m.content }]
                });
            } else if (m.role === 'assistant' && m.tool_calls) {
                var blocks = [];
                if (m.content) blocks.push({ type: 'text', text: m.content });
                m.tool_calls.forEach(function (tc) {
                    blocks.push({
                        type: 'tool_use',
                        id: tc.id,
                        name: tc.function.name,
                        input: JSON.parse(tc.function.arguments || '{}')
                    });
                });
                anthMsgs.push({ role: 'assistant', content: blocks });
            } else if (m.role === 'user' || m.role === 'assistant') {
                anthMsgs.push({ role: m.role, content: m.content || '' });
            }
        });
        var body = {
            model: cfg.model,
            max_tokens: cfg.maxTokens,
            system: sys || systemPrompt(),
            messages: anthMsgs
        };
        if (!opts.chatOnly) {
            body.tools = TOOLS.map(function (t) {
                return {
                    name: t.function.name,
                    description: t.function.description,
                    input_schema: t.function.parameters || { type: 'object', properties: {} }
                };
            });
        }
        var r = await fetchWithTimeout(cfg.url, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
                'x-api-key': cfg.key,
                'anthropic-version': '2023-06-01',
                'anthropic-dangerous-direct-browser-access': 'true'
            },
            body: JSON.stringify(body)
        }, 45000, '模型请求');
        var text = await r.text();
        if (!r.ok) throw new Error('HTTP ' + r.status + ': ' + text.slice(0, 400));
        var d = JSON.parse(text);
        var out = normalizeAnthropicMessage(d);
        out.usage = d.usage || null;
        out.model = d.model || cfg.model;
        return out;
    }

    function normalizeOpenAIMessage(msg) {
        var toolCalls = (msg.tool_calls || []).map(function (tc) {
            return {
                id: tc.id,
                name: tc.function.name,
                arguments: safeParseArgs(tc.function.arguments)
            };
        });
        if (!toolCalls.length && msg.content) {
            var parsedText = parseTextToolProtocol(msg.content);
            parsedText.raw = msg;
            return parsedText;
        }
        return { role: 'assistant', content: msg.content || '', tool_calls: toolCalls, raw: msg };
    }

    function normalizeAnthropicMessage(d) {
        var content = '';
        var toolCalls = [];
        (d.content || []).forEach(function (block) {
            if (block.type === 'text') content += block.text;
            if (block.type === 'tool_use') {
                toolCalls.push({ id: block.id, name: block.name, arguments: block.input || {} });
            }
        });
        return { role: 'assistant', content: content, tool_calls: toolCalls, raw: d };
    }

    function parseTextToolProtocol(content) {
        if (!window.QuantAgentProtocol) return { role: 'assistant', content: String(content || ''), tool_calls: [] };
        return window.QuantAgentProtocol.parseToolProtocol(content);
    }

    function safeParseArgs(s) {
        if (typeof s === 'object' && s) return s;
        try { return JSON.parse(s || '{}'); } catch (e) { return {}; }
    }

    async function agentLoop(userText) {
        if (agentBusy) return;
        abortFlag = false;

        appendText('user', userText);

        // ① 本地直答：不问模型（会明确标注，避免被当成「假 AI」）
        var quick = tryQuickReply(userText);
        if (quick) {
            appendText(
                'assistant',
                quick,
                null,
                '本地直答 · 未调用 API'
            );
            history.push({ role: 'user', content: userText });
            history.push({ role: 'assistant', content: quick });
            updateContextMeter();
            return;
        }

        // 手动 compact 指令
        if (/^(compact|压缩|压缩对话|压缩上下文)\s*$/i.test(String(userText || '').trim())) {
            setBusy(true);
            try {
                var cr = await compactHistory(true);
                if (!cr.did) {
                    appendText('system', '无需 Compact（' + (cr.reason || 'ok') + '）· 估算 '
                        + (cr.meter && cr.meter.est != null ? cr.meter.est.toLocaleString() : '—')
                        + ' tokens', 'step-warn');
                }
            } catch (e) {
                appendText('system', 'Compact 失败: ' + e.message, 'step-err');
            }
            setBusy(false);
            updateContextMeter();
            return;
        }

        var research = looksLikeResearchTask(userText);
        var executedToolCalls = Object.create(null);
        setBusy(true);
        history.push({ role: 'user', content: userText });

        // system 只放一次；研究任务才附带 workspace，闲聊不塞整份配置
        if (!history.length || history[0].role !== 'system') {
            history.unshift({ role: 'system', content: systemPrompt() });
        } else {
            // 刷新 system 里的模型信息（用户可能刚改了模型）
            history[0] = { role: 'system', content: systemPrompt() };
        }
        if (research) {
            history.push({
                role: 'user',
                content: '[workspace] ' + JSON.stringify(workspace())
            });
        }

        function metaFromReply(reply, modeLabel) {
            var cfg = getProvider();
            var bits = [modeLabel || '真·模型调用', reply && reply.model ? reply.model : cfg.model];
            var u = formatUsage(reply && reply.usage);
            if (u) bits.push(u);
            var meter = updateContextMeter();
            bits.push('ctx ~' + Math.round(meter.ratio * 100) + '%');
            return bits.filter(Boolean).join(' · ');
        }

        try {
            await ensureContextBudget();

            if (!research) {
                // ② 轻量闲聊：单次真 LLM，不带 tools
                var chatReply = await callModel(history, { chatOnly: true });
                var chatText = (chatReply.content || '').trim() || '(empty)';
                appendText('assistant', chatText, null, metaFromReply(chatReply, '真·模型调用'));
                history.push({ role: 'assistant', content: chatText });
                updateContextMeter();
                setBusy(false);
                return;
            }

            // ③ 研究任务：工具循环（每步会标模型与 token 用量）
            for (var step = 0; step < MAX_STEPS; step++) {
                if (abortFlag) {
                    appendText('system', '已停止。', 'step-warn');
                    break;
                }
                await ensureContextBudget();
                appendText('system', '研究步骤 ' + (step + 1) + '/' + MAX_STEPS + ' · 正在请求模型…', 'step-ai');
                var reply = await callModel(history, { chatOnly: false });
                if (reply.tool_calls && reply.tool_calls.length) {
                    history.push({
                        role: 'assistant',
                        content: reply.content || null,
                        tool_calls: reply.tool_calls.map(function (tc) {
                            return {
                                id: tc.id,
                                type: 'function',
                                function: { name: tc.name, arguments: JSON.stringify(tc.arguments || {}) }
                            };
                        })
                    });
                    if (reply.content) {
                        appendText('assistant', reply.content, 'step-ai', metaFromReply(reply, '真·模型+工具'));
                    } else {
                        appendText(
                            'system',
                            '模型决定调用 ' + reply.tool_calls.length + ' 个工具 · ' + (formatUsage(reply.usage) || 'usage n/a'),
                            'step-ai'
                        );
                    }

                    for (var i = 0; i < reply.tool_calls.length; i++) {
                        if (abortFlag) break;
                        var tc = reply.tool_calls[i];
                        var callKey = tc.name + ':' + JSON.stringify(tc.arguments || {});
                        var exec;
                        if (executedToolCalls[callKey]) {
                            exec = { ok: false, result: { status: 'error', message: 'duplicate identical tool call blocked; use the previous result' } };
                            appendTool(tc.name, tc.arguments || {}, exec.result, false);
                        } else {
                            executedToolCalls[callKey] = true;
                            exec = await executeTool(tc.name, tc.arguments || {});
                        }
                        history.push({
                            role: 'tool',
                            tool_call_id: tc.id,
                            content: JSON.stringify(exec.result)
                        });
                    }
                    continue;
                }

                var finalText = reply.content || '(empty)';
                appendText('assistant', finalText, null, metaFromReply(reply, '真·模型调用'));
                history.push({ role: 'assistant', content: finalText });
                break;
            }
        } catch (e) {
            appendText('system', '错误: ' + e.message, 'step-err');
        }
        updateContextMeter();
        refreshAgentGovernance();
        setBusy(false);
    }

    function bindUI() {
        chatEl = $('agent-chat');
        var send = $('agent-send');
        var input = $('agent-input');
        var stop = $('agent-stop');
        var compactBtn = $('agent-compact');
        var missionRun = $('mission-run');
        if (send && input) {
            send.addEventListener('click', function () {
                var text = (input.value || '').trim();
                if (!text) return;
                input.value = '';
                agentLoop(text);
            });
            input.addEventListener('keydown', function (e) {
                if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                    e.preventDefault();
                    send.click();
                }
            });
        }
        if (stop) {
            stop.addEventListener('click', function () {
                abortFlag = true;
                if (activeRequestController) activeRequestController.abort();
            });
        }
        if (compactBtn) {
            compactBtn.addEventListener('click', function () {
                if (agentBusy) return;
                agentLoop('compact');
            });
        }
        if (missionRun) missionRun.addEventListener('click', startStrategyMission);
        ['ai-context-window', 'ai-max-tokens'].forEach(function (id) {
            var el = $(id);
            if (el) {
                el.addEventListener('change', updateContextMeter);
                el.addEventListener('input', updateContextMeter);
            }
        });
        document.querySelectorAll('[data-agent-prompt]').forEach(function (btn) {
            btn.addEventListener('click', function () {
                var p = btn.getAttribute('data-agent-prompt');
                if (p) agentLoop(p);
            });
        });
        updateContextMeter();
    }

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', bindUI);
    } else {
        bindUI();
    }

    window.QuantAgent = {
        run: agentLoop,
        stop: function () {
            abortFlag = true;
            if (activeRequestController) activeRequestController.abort();
        },
        compact: function () { return compactHistory(true); },
        contextMeter: updateContextMeter,
        tools: TOOLS,
        research: startStrategyMission,
        scoreStrategyCandidate: scoreStrategyCandidate,
        // 暴露给自动化测试：验证"拒绝时不写实盘"这条不变量，不必真的接一个大模型
        _executeTool: executeTool
    };
})();
