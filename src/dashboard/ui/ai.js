// AI Strategy Lab - Frontend Logic
(function() {
    'use strict';

    var lastBacktestResult = null;
    var lastAiThought = null; // { model, goal, params, text, prompt }

    // ── Provider presets: url + model defaults ──
    var PRESETS = {
        openai:   { url: 'https://api.openai.com/v1/chat/completions',          model: 'gpt-4o' },
        zhipu:    { url: 'https://open.bigmodel.cn/api/paas/v4/chat/completions', model: 'glm-4-plus' },
        deepseek: { url: 'https://api.deepseek.com/v1/chat/completions',        model: 'deepseek-chat' },
        claude:   { url: 'https://api.anthropic.com/v1/messages',               model: 'claude-sonnet-4-20250514' },
        groq:     { url: 'https://api.groq.com/openai/v1/chat/completions',     model: 'llama-3.3-70b-versatile' },
        ollama:   { url: 'http://localhost:11434/v1/chat/completions',           model: 'llama3' },
        dadunode: { url: 'https://dadunode.com:8443/v1/chat/completions', model: '' },
        custom:   { url: '',                                                     model: '' }
    };

    // ── Non-secret LocalStorage persistence ──
    var STORAGE_KEY = 'lighter-ai-settings';
    // 与主面板共用 lighter-lang，进入 /ai 时语言保持一致
    var LANG_KEY = 'lighter-lang';

    // ── i18n：主面板有完整中英，AI Lab 以前是硬编码英文独立页 ──
    var I18N = {
        en: {
            brandKicker: 'BACKTEST IN · PARAMETERS OUT · NO LIVE ORDERS',
            brandEm: ' · strategy lab',
            pillLab: 'STRATEGY LAB',
            pillSandbox: 'SANDBOX',
            navDashboard: 'Dashboard', navStrategies: 'Strategies', navPortfolio: 'Portfolio',
            navHistory: 'History', navSettings: 'Settings', navAiLab: 'AI Lab',
            btConfig: 'Backtest Configuration', strategy: 'Strategy',
            optGrid: 'Grid Trading', optTrend: 'Trend Following', optDca: 'DCA (Dollar-Cost Averaging)',
            dataFile: 'Data File', startDate: 'Start Date', endDate: 'End Date',
            initialCapital: 'Initial Capital ($)', strategyParams: 'Strategy Parameters',
            runBacktest: 'Run Backtest', running: 'Running...',
            aiAdvisor: 'AI Strategy Advisor', aiProvider: 'AI Provider (Preset)',
            optCustom: 'Custom / Other', apiBaseUrl: 'API Base URL', modelId: 'Model ID',
            apiKey: 'API Key',
            apiKeyHint: 'Saved in this browser’s localStorage so it survives refresh. Only visible on this machine; not uploaded to the trading server.',
            dataFileHint: 'Dates auto-align to the selected file’s real first/last candle.',
            optGoal: 'Optimization Goal',
            goalSharpe: 'Maximize Sharpe Ratio', goalReturn: 'Maximize Return',
            goalDrawdown: 'Minimize Max Drawdown', goalBalanced: 'Balanced (Return + Risk)',
            maxTokens: 'Max Tokens',
            btnOptimize: 'AI Optimize with Your Model', btnTest: 'Test',
            primaryTag: 'PRIMARY',
            primaryNote: 'Quant optimization uses the API Provider / Model ID / API Key you configure above. The browser calls that endpoint directly — nothing is forced to OpenCode or GLM5.',
            noteTag: 'NOTE',
            noteText: 'Non-secret settings auto-save to this browser. The API key stays in memory for this page session only.',
            opencodeSummary: 'Optional · Local OpenCode CLI',
            opencodeModel: 'OpenCode Model (only if you use the local CLI)',
            btnOpencode: 'Run OpenCode Optimize + Backtest',
            opencodeNote: 'Optional backend path that shells out to a locally installed opencode CLI. Leave this closed unless you intentionally want that flow — normal quant work should use your API model above.',
            results: 'Backtest Results',
            resultsEmpty: 'Run a backtest or AI optimization to see results here',
            resultsEmptyHint: 'Configure on the left, then run. The right panel streams steps, model reasoning, and verified metrics.',
            rightTitle: 'AI Backtest Process & Results',
            processTitle: 'Run Log',
            processLive: 'Running',
            thinkTitle: 'AI Reasoning / Verified Board',
            thinkEmpty: 'AI reasoning and verified candidates will show here',
            togglePrompt: 'Show prompt',
            hidePrompt: 'Hide prompt',
            thinkModel: 'Model',
            thinkGoal: 'Goal',
            thinkParams: 'Best params',
            processEmpty: 'Waiting to run…',
            btnSweep: 'Local param sweep (no AI)',
            sweepMode: 'Sweep depth',
            modeQuick: 'Quick (~80 combos)',
            modeFull: 'Full (more combos, slower)',
            sweepOptionalSummary: 'Optional · Local grid search (no AI)',
            sweepNoteTag: 'LOCAL',
            sweepNote: 'Does not call your AI key. Exhaustive grid only.',
            primaryTag: 'PRIMARY',
            primaryNote: 'Uses your API key to drive multi-round param proposals. Every candidate is verified by the real backtest engine before the next AI round.',
            noteText: 'The key stays in this browser and is sent only to your AI endpoint. The trading bot never sees the key — it only runs backtests.',
            btnOptimize: 'AI-driven backtest optimize',
            aiRounds: 'AI rounds',
            aiCandidates: 'Candidates / round',
            leaderboard: 'Verified leaderboard',
            sweepStart: 'Starting local param sweep…',
            sweepDone: 'Sweep complete',
            noProfitable: 'No profitable combo on this window — try another dataset/range or strategy.',
            aiLoopStart: 'Starting AI-driven backtest loop…',
            aiRound: 'AI round',
            aiPropose: 'Asking your model for candidates…',
            aiVerify: 'Verifying candidate via backtest engine…',
            aiNoCandidate: 'Model returned no parseable PARAMS lines',
            aiBest: 'Best verified so far',
            needUrl: 'Please enter an API Base URL',
            needModel: 'Please enter a Model ID',
            needKey: 'Please enter an API Key',
            connOk: 'Connection OK',
            connFail: 'Connection failed:',
            reqFail: 'Request failed: ',
            errPrefix: 'Error: ',
            analyzing: 'Analyzing...',
            profit: 'PROFIT', loss: 'LOSS',
            backtestOn: 'Backtest: ',
            onFile: ' on ',
            mTotalReturn: 'Total Return', mSharpe: 'Sharpe Ratio', mMaxDd: 'Max Drawdown',
            mPf: 'Profit Factor', mTrades: 'Total Trades', mFinalEq: 'Final Equity',
            mAvgWin: 'Avg Win', mAvgLoss: 'Avg Loss',
            thTime: 'Time', thSide: 'Side', thPrice: 'Price', thSize: 'Size', thPnl: 'PnL',
            applyLive: 'Apply to Live Trading',
            applyLiveHint: 'Applies', applyLiveHint2: 'with params:', applyLiveHint3: 'to your live bot',
            noParams: 'No parameters to apply',
            applying: 'Applying...', applied: 'Applied to Live', applyFail: 'Failed - Try Again',
            failApply: 'Failed to apply: ',
            logStart: 'Starting AI optimization...',
            logBaseline: 'Running baseline backtest: ',
            logConsulting: 'Consulting AI for suggestions...',
            logAiResp: 'AI response received',
            logSuggested: 'Suggested params: ',
            logRebt: 'Running backtest with AI params...',
            logParseFail: 'Could not parse params from AI response',
            logImproved: 'Improvement: ',
            logOpt: 'AI Optimized: Return=',
            logBase: 'Baseline: Return=',
            logRange: 'Backtest window: ',
            logBaselineLabel: 'Baseline backtest',
            logOptLabel: 'Optimized backtest',
            logNoTrades: 'Baseline produced 0 trades — AI advice is weak on empty samples. Check data range / params.',
            logVerified: 'Verified: both legs ran through /api/backtest (not live trading). Use “Apply to Live” only if you accept the result.',
            btFail: 'Backtest failed',
            btEmpty: 'Backtest returned empty metrics',
            vsBaseline: 'vs baseline',
            baselineCard: 'Baseline',
            optimizedCard: 'AI Optimized',
            sandboxNote: 'Sandbox only — no live orders were placed. Click Apply to push params to the running bot.',
            needOcModel: 'OpenCode is optional. Enter a local OpenCode model string, or use “AI Optimize with Your Model” with your API settings instead.',
            ocStart: 'Starting optional OpenCode optimize + backtest...',
            ocFail: 'OpenCode optimize failed',
            ocReqFail: 'OpenCode request failed: '
        },
        cn: {
            brandKicker: '回测输入 · 参数输出 · 不下实盘单',
            brandEm: ' · 策略实验室',
            pillLab: '策略实验室',
            pillSandbox: '沙箱',
            navDashboard: '总览', navStrategies: '策略', navPortfolio: '资产',
            navHistory: '历史', navSettings: '设置', navAiLab: 'AI 实验室',
            btConfig: '回测配置', strategy: '策略',
            optGrid: '网格交易', optTrend: '趋势跟踪', optDca: '定投 (DCA)',
            dataFile: '数据文件', startDate: '开始日期', endDate: '结束日期',
            initialCapital: '初始资金 ($)', strategyParams: '策略参数',
            runBacktest: '运行回测', running: '运行中...',
            aiAdvisor: 'AI 策略顾问', aiProvider: 'AI 提供商（预设）',
            optCustom: '自定义 / 其他', apiBaseUrl: 'API 地址', modelId: '模型 ID',
            apiKey: 'API 密钥',
            apiKeyHint: '保存在本浏览器 localStorage，刷新后仍可用；仅本机可见，不会上传到交易服务器。',
            dataFileHint: '日期会随所选数据文件自动对齐到该文件真实起止。',
            optGoal: '优化目标',
            goalSharpe: '最大化夏普比率', goalReturn: '最大化收益',
            goalDrawdown: '最小化最大回撤', goalBalanced: '收益与风险平衡',
            maxTokens: '最大 Token',
            btnOptimize: '用你的模型做 AI 优化', btnTest: '测试连接',
            primaryTag: '主路径',
            primaryNote: '量化优化使用你上方配置的 API 提供商 / 模型 / 密钥。浏览器直接请求该接口，不会强制走 OpenCode 或 GLM5。',
            noteTag: '说明',
            noteText: '非敏感设置会自动保存到本浏览器。API 密钥只留在当前页面会话内存中。',
            opencodeSummary: '可选 · 本地 OpenCode CLI',
            opencodeModel: 'OpenCode 模型（仅本地 CLI）',
            btnOpencode: '运行 OpenCode 优化 + 回测',
            opencodeNote: '可选后端路径：由本机已安装的 opencode CLI 建议参数后再跑回测。日常量化请用上方你自己的 API 模型。',
            results: '回测结果',
            resultsEmpty: '运行回测或 AI 优化后，结果会显示在这里',
            resultsEmptyHint: '左侧配置参数 → 运行优化。右侧会同步展示步骤、模型推理与验证后的回测指标。',
            rightTitle: 'AI 回测过程与结果',
            processTitle: '运行过程',
            processLive: '进行中',
            thinkTitle: 'AI 思考 / 已验证榜单',
            thinkEmpty: 'AI 推理与已验证候选会出现在这里',
            togglePrompt: '看提示词',
            hidePrompt: '收起提示词',
            thinkModel: '模型',
            thinkGoal: '目标',
            thinkParams: '当前最优参数',
            processEmpty: '等待运行…',
            btnSweep: '本地参数扫描（不走 AI）',
            sweepMode: '扫描深度',
            modeQuick: '快速（约 80 组）',
            modeFull: '完整（更多组合，更慢）',
            sweepOptionalSummary: '可选 · 本地穷举扫描（不走 AI）',
            sweepNoteTag: '本地',
            sweepNote: '不调用你的 API Key，只做网格穷举。',
            primaryTag: '主路径',
            primaryNote: '用你的 API Key 调用大模型；模型根据真实回测结果多轮提议参数，每一组都经本机回测引擎验证后再进入下一轮。',
            noteText: '密钥只存在本浏览器，请求直连你配置的 AI 接口；交易机器人只负责跑回测，收不到密钥。',
            btnOptimize: 'AI 驱动回测优化',
            aiRounds: 'AI 轮次',
            aiCandidates: '每轮候选数',
            leaderboard: '已验证参数榜单',
            sweepStart: '开始本地参数扫描…',
            sweepDone: '扫描完成',
            noProfitable: '该区间没有正收益组合 — 可换数据/区间/策略再试。',
            aiLoopStart: '开始 AI 驱动回测闭环…',
            aiRound: 'AI 第',
            aiPropose: '正在用你的模型提出候选参数…',
            aiVerify: '正在用回测引擎验证候选…',
            aiNoCandidate: '模型未返回可解析的 PARAMS 行',
            aiBest: '当前已验证最优',
            needUrl: '请填写 API 地址',
            needModel: '请填写模型 ID',
            needKey: '请填写 API 密钥',
            connOk: '连接成功',
            connFail: '连接失败：',
            reqFail: '请求失败：',
            errPrefix: '错误：',
            analyzing: '分析中...',
            profit: '盈利', loss: '亏损',
            backtestOn: '回测：',
            onFile: ' · 数据 ',
            mTotalReturn: '总收益', mSharpe: '夏普比率', mMaxDd: '最大回撤',
            mPf: '盈利因子', mTrades: '成交笔数', mFinalEq: '最终净值',
            mAvgWin: '平均盈利', mAvgLoss: '平均亏损',
            thTime: '时间', thSide: '方向', thPrice: '价格', thSize: '数量', thPnl: '盈亏',
            applyLive: '应用到实盘',
            applyLiveHint: '将', applyLiveHint2: '参数：', applyLiveHint3: '写入实盘机器人',
            noParams: '没有可应用的参数',
            applying: '应用中...', applied: '已应用到实盘', applyFail: '失败，请重试',
            failApply: '应用失败：',
            logStart: '开始 AI 优化...',
            logBaseline: '运行基线回测：',
            logConsulting: '正在向 AI 咨询参数建议...',
            logAiResp: '已收到 AI 回复',
            logSuggested: '建议参数：',
            logRebt: '使用 AI 参数重新回测...',
            logParseFail: '无法从 AI 回复中解析参数',
            logImproved: '相对提升：',
            logOpt: 'AI 优化后：收益=',
            logBase: '基线：收益=',
            logRange: '回测区间：',
            logBaselineLabel: '基线回测',
            logOptLabel: '优化后回测',
            logNoTrades: '基线 0 笔成交 — 空样本上 AI 建议参考价值低。请检查数据区间/参数。',
            logVerified: '已验证：基线与优化参数都经 /api/backtest 实盘引擎回测（未下实盘单）。若要上线请点「应用到实盘」。',
            btFail: '回测失败',
            btEmpty: '回测未返回有效指标',
            vsBaseline: '相对基线',
            baselineCard: '基线',
            optimizedCard: 'AI 优化后',
            sandboxNote: '仅沙箱回测 — 未对实盘下单。确认结果后再点「应用到实盘」。',
            needOcModel: 'OpenCode 为可选功能。请填写本地 OpenCode 模型名，或改用上方「用你的模型做 AI 优化」。',
            ocStart: '开始可选的 OpenCode 优化 + 回测...',
            ocFail: 'OpenCode 优化失败',
            ocReqFail: 'OpenCode 请求失败：'
        }
    };

    // 与主面板同一把 key；无记录时默认中文（本页目标用户以中文为主）
    var currentLang = localStorage.getItem(LANG_KEY) || 'cn';
    if (currentLang !== 'en' && currentLang !== 'cn') currentLang = 'cn';

    function t(key) {
        var pack = I18N[currentLang] || I18N.cn;
        return pack[key] || I18N.en[key] || key;
    }

    function applyI18n() {
        document.documentElement.lang = currentLang === 'cn' ? 'zh-CN' : 'en';
        document.querySelectorAll('[data-i18n]').forEach(function(el) {
            var key = el.getAttribute('data-i18n');
            var val = t(key);
            if (val) el.textContent = val;
        });
        var label = document.getElementById('ai-lang-label');
        if (label) label.textContent = currentLang === 'en' ? 'EN' : '中';
        // 按钮默认文案（未处于 loading 状态时）
        var rb = document.getElementById('btn-run-backtest');
        if (rb && !rb.disabled) rb.textContent = t('runBacktest');
        var sb = document.getElementById('btn-sweep-optimize');
        if (sb && !sb.disabled) sb.textContent = t('btnSweep');
        var ob = document.getElementById('btn-ai-optimize');
        if (ob && !ob.disabled) ob.textContent = t('btnOptimize');
        var tb = document.getElementById('btn-ai-test');
        if (tb && !tb.disabled) tb.textContent = t('btnTest');
        var ocb = document.getElementById('btn-opencode-optimize');
        if (ocb && !ocb.disabled) ocb.textContent = t('btnOpencode');
        var empty = document.getElementById('results-empty');
        if (empty && empty.style.display !== 'none') {
            // empty block has two spans
            var main = empty.querySelector('[data-i18n="resultsEmpty"]') || empty;
            if (main) main.textContent = t('resultsEmpty');
            var hint = empty.querySelector('[data-i18n="resultsEmptyHint"]');
            if (hint) hint.textContent = t('resultsEmptyHint');
        }
        var stream = document.getElementById('process-stream');
        if (stream) stream.setAttribute('data-empty', t('processEmpty'));
        var te = document.getElementById('think-empty');
        if (te) te.textContent = t('thinkEmpty');
        var tp = document.getElementById('btn-toggle-prompt');
        if (tp && !tp.dataset.open) tp.textContent = t('togglePrompt');
        // 若已有结果，用当前语言重绘
        if (lastBacktestResult) renderResults(lastBacktestResult);
        if (lastAiThought) renderThought(lastAiThought);
    }

    function toggleLang() {
        currentLang = currentLang === 'en' ? 'cn' : 'en';
        try { localStorage.setItem(LANG_KEY, currentLang); } catch (e) {}
        applyI18n();
    }

    function escapeHtml(value) {
        return String(value == null ? '' : value)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    // 补全 token 上限：旧版/误填会把 1000000 写进 localStorage，
    // 多数模型 completion 上限远低于此（报错示例：at most 131072）。
    // 单次 completion 上限（与 Agent 页面对齐；对话上下文 100 万另字段）
    var MAX_TOKENS_MIN = 256;
    var MAX_TOKENS_MAX = 131072;
    var MAX_TOKENS_DEFAULT = 32768;

    function clampMaxTokens(raw) {
        var n = parseInt(raw, 10);
        if (!Number.isFinite(n) || n <= 0) return MAX_TOKENS_DEFAULT;
        if (n < MAX_TOKENS_MIN) return MAX_TOKENS_MIN;
        if (n > MAX_TOKENS_MAX) return MAX_TOKENS_MAX;
        return n;
    }

    function readMaxTokensFromUi() {
        var el = document.getElementById('ai-max-tokens');
        var clamped = clampMaxTokens(el ? el.value : MAX_TOKENS_DEFAULT);
        if (el && String(el.value) !== String(clamped)) el.value = String(clamped);
        return clamped;
    }

    function saveSettings() {
        var settings = {
            provider: document.getElementById('ai-provider').value,
            url: document.getElementById('ai-url').value,
            model: document.getElementById('ai-model').value,
            // 用户明确要求持久化 API Key（仅本浏览器 localStorage）
            key: document.getElementById('ai-key').value,
            goal: document.getElementById('ai-goal').value,
            maxTokens: String(readMaxTokensFromUi()),
            contextWindow: (document.getElementById('ai-context-window') || {}).value || '1000000',
            opencodeModel: document.getElementById('opencode-model').value,
            dataFile: document.getElementById('bt-data').value,
            start: document.getElementById('bt-start').value,
            end: document.getElementById('bt-end').value,
            capital: document.getElementById('bt-capital').value,
            params: document.getElementById('bt-params').value,
            rounds: (document.getElementById('ai-rounds') || {}).value || '3',
            candidates: (document.getElementById('ai-candidates') || {}).value || '3'
        };
        try { localStorage.setItem(STORAGE_KEY, JSON.stringify(settings)); } catch(e) {}
    }

    function loadSettings() {
        try {
            var raw = localStorage.getItem(STORAGE_KEY);
            if (!raw) return null;
            var s = JSON.parse(raw);
            if (s.provider) document.getElementById('ai-provider').value = s.provider;
            if (s.url) document.getElementById('ai-url').value = s.url;
            if (s.model) document.getElementById('ai-model').value = s.model;
            if (s.key) document.getElementById('ai-key').value = s.key;
            if (s.goal) document.getElementById('ai-goal').value = s.goal;
            if (s.maxTokens != null && s.maxTokens !== '') {
                var fixed = clampMaxTokens(s.maxTokens);
                // 旧版若把「100 万上下文」误存进 maxTokens，迁到 context window
                if (Number(s.maxTokens) >= 200000 && document.getElementById('ai-context-window')) {
                    if (!s.contextWindow) {
                        document.getElementById('ai-context-window').value = String(Math.min(2000000, Number(s.maxTokens)));
                    }
                    fixed = MAX_TOKENS_DEFAULT;
                }
                document.getElementById('ai-max-tokens').value = String(fixed);
                if (String(s.maxTokens) !== String(fixed)) {
                    s.maxTokens = String(fixed);
                    localStorage.setItem(STORAGE_KEY, JSON.stringify(s));
                }
            }
            if (s.contextWindow && document.getElementById('ai-context-window')) {
                document.getElementById('ai-context-window').value = String(s.contextWindow);
            }
            if (s.opencodeModel) document.getElementById('opencode-model').value = s.opencodeModel;
            if (s.capital) document.getElementById('bt-capital').value = s.capital;
            if (s.params) document.getElementById('bt-params').value = s.params;
            if (s.rounds && document.getElementById('ai-rounds')) document.getElementById('ai-rounds').value = s.rounds;
            if (s.candidates && document.getElementById('ai-candidates')) document.getElementById('ai-candidates').value = s.candidates;
            return s;
        } catch(e) {
            return null;
        }
    }

    // Load saved settings on startup
    var savedSettings = loadSettings();

    // Prefer the user-configured provider path. If URL/model are empty on first
    // visit, fill from the selected preset — never auto-select OpenCode/GLM5.
    (function applyProviderDefaults() {
        var providerEl = document.getElementById('ai-provider');
        var urlEl = document.getElementById('ai-url');
        var modelEl = document.getElementById('ai-model');
        if (!providerEl || !urlEl || !modelEl) return;
        var p = PRESETS[providerEl.value] || PRESETS.openai;
        if (!urlEl.value) urlEl.value = p.url;
        if (!modelEl.value && p.model) modelEl.value = p.model;
        // Clear legacy default that made OpenCode look like the primary path
        var oc = document.getElementById('opencode-model');
        if (oc && oc.value === 'opencode-go/glm-5') {
            // Keep only if the user explicitly saved it; otherwise blank.
            try {
                var raw = localStorage.getItem(STORAGE_KEY);
                var s = raw ? JSON.parse(raw) : null;
                if (!s || !s.opencodeModel) oc.value = '';
            } catch (e) {
                oc.value = '';
            }
        }
    })();

    // Auto-save on any input change (including API key)
    ['ai-provider','ai-url','ai-model','ai-key','ai-goal','ai-max-tokens','ai-context-window','ai-rounds','ai-candidates','opencode-model',
     'bt-data','bt-start','bt-end','bt-capital','bt-params'].forEach(function(id) {
        var el = document.getElementById(id);
        if (el) el.addEventListener('change', saveSettings);
        if (el) el.addEventListener('input', saveSettings);
    });

    // Provider dropdown → fill URL + model from preset
    document.getElementById('ai-provider').addEventListener('change', function() {
        var preset = PRESETS[this.value];
        if (preset) {
            document.getElementById('ai-url').value = preset.url;
            document.getElementById('ai-model').value = preset.model;
            saveSettings();
        }
    });

    // ── 数据集列表 + 日期自动对齐 ──
    var DATASET_MAP = {}; // file -> {start,end,candles,label}

    function applyDatasetDates(file, force) {
        var meta = DATASET_MAP[file];
        if (!meta || !meta.start || !meta.end) return;
        var startEl = document.getElementById('bt-start');
        var endEl = document.getElementById('bt-end');
        if (!startEl || !endEl) return;
        // force=true：换文件时始终对齐；否则仅在空/越界时修正
        if (force || !startEl.value || startEl.value < meta.start || startEl.value > meta.end) {
            startEl.value = meta.start;
        }
        if (force || !endEl.value || endEl.value > meta.end || endEl.value < meta.start) {
            endEl.value = meta.end;
        }
        // 保证 start <= end
        if (startEl.value > endEl.value) {
            startEl.value = meta.start;
            endEl.value = meta.end;
        }
        var hint = document.getElementById('bt-data-hint');
        if (hint && meta.candles) {
            hint.textContent = t('dataFileHint') + ' · ' + meta.start + ' → ' + meta.end + ' · ' + meta.candles + ' bars';
        }
    }

    function loadDatasets() {
        var sel = document.getElementById('bt-data');
        if (!sel) return Promise.resolve();
        return fetch('/api/backtest/datasets')
            .then(function(r) { return r.json(); })
            .then(function(data) {
                var list = (data && data.datasets) || [];
                if (!list.length) {
                    sel.innerHTML = '<option value="BTC-synthetic-30d-1h.csv">BTC-synthetic-30d-1h.csv (fallback)</option>';
                    return;
                }
                DATASET_MAP = {};
                sel.innerHTML = '';
                list.forEach(function(ds) {
                    if (!ds.file) return;
                    DATASET_MAP[ds.file] = {
                        start: ds.start || '',
                        end: ds.end || '',
                        candles: ds.candles || 0,
                        label: ds.label || ds.file
                    };
                    var opt = document.createElement('option');
                    opt.value = ds.file;
                    opt.textContent = ds.label || ds.file;
                    sel.appendChild(opt);
                });

                // 选择优先级：用户保存的文件 → 服务端 default（最新）→ 第一项
                var preferred = (savedSettings && savedSettings.dataFile) || data.default || list[0].file;
                if (!DATASET_MAP[preferred]) preferred = list[0].file;
                sel.value = preferred;

                // 若用户保存过日期且仍在文件范围内则保留，否则强制对齐
                var meta = DATASET_MAP[preferred] || {};
                var savedStart = savedSettings && savedSettings.start;
                var savedEnd = savedSettings && savedSettings.end;
                var keepSaved = savedStart && savedEnd && meta.start && meta.end
                    && savedStart >= meta.start && savedEnd <= meta.end && savedStart <= savedEnd;
                if (keepSaved) {
                    document.getElementById('bt-start').value = savedStart;
                    document.getElementById('bt-end').value = savedEnd;
                    applyDatasetDates(preferred, false);
                } else {
                    applyDatasetDates(preferred, true);
                }
                saveSettings();
            })
            .catch(function() {
                sel.innerHTML = '<option value="BTC-synthetic-30d-1h.csv">BTC-synthetic-30d-1h.csv</option>';
            });
    }

    var btDataEl = document.getElementById('bt-data');
    if (btDataEl) {
        btDataEl.addEventListener('change', function() {
            applyDatasetDates(this.value, true);
            saveSettings();
        });
    }
    loadDatasets();

    // 切换策略时给出可成交的默认参数（趋势必须带 notional，且 ≤ 资金）
    (function bindStrategyDefaults() {
        var stratEl = document.getElementById('bt-strategy');
        var paramsEl = document.getElementById('bt-params');
        var capitalEl = document.getElementById('bt-capital');
        if (!stratEl || !paramsEl) return;
        function defaultParamsFor(strategy) {
            var cap = parseFloat(capitalEl && capitalEl.value) || 125;
            var n = Math.max(10, Math.min(cap * 0.5, cap * 0.9));
            if (strategy === 'trend' || strategy === 'trend_following') {
                return 'fast_ma=14,slow_ma=50,stop_loss=0.05,take_profit=0.06,trailing_stop=0,notional=' + n.toFixed(2);
            }
            if (strategy === 'dca') {
                var amt = Math.max(1, Math.min(cap * 0.2, 20));
                return 'interval=4,amount=' + amt.toFixed(2) + ',dip_threshold=2';
            }
            return 'grid_count=10,investment=8,deviation=0.012';
        }
        stratEl.addEventListener('change', function() {
            // 仅在参数像上一策略残留时覆盖，避免抹掉用户自定义
            var p = (paramsEl.value || '').trim();
            var s = stratEl.value;
            var looksGrid = /grid_count|investment|deviation/.test(p);
            var looksTrend = /fast_ma|slow_ma|notional/.test(p);
            var looksDca = /interval|dip_threshold/.test(p);
            var mismatch =
                (s === 'grid' && (looksTrend || looksDca) && !looksGrid) ||
                ((s === 'trend' || s === 'trend_following') && (looksGrid || looksDca) && !looksTrend) ||
                (s === 'dca' && (looksGrid || looksTrend) && !looksDca) ||
                !p;
            // 趋势参数缺 notional 也强制补全
            if ((s === 'trend' || s === 'trend_following') && p && !/notional=/.test(p)) {
                mismatch = true;
            }
            if (mismatch) {
                paramsEl.value = defaultParamsFor(s);
                saveSettings();
            }
        });
    })();

    // Toggle API key visibility
    document.getElementById('toggle-key-vis').addEventListener('click', function() {
        var inp = document.getElementById('ai-key');
        var isPassword = inp.type === 'password';
        inp.type = isPassword ? 'text' : 'password';
        var icon = this.querySelector('[data-lucide]');
        if (icon) {
            icon.setAttribute('data-lucide', isPassword ? 'eye-off' : 'eye');
            lucide.createIcons();
        }
    });

    // ── 右侧过程流 / 思考卡 ──
    function setProcessLive(on) {
        var el = document.getElementById('process-live');
        if (el) el.style.display = on ? 'inline-flex' : 'none';
    }

    function clearProcessStream() {
        var stream = document.getElementById('process-stream');
        if (stream) stream.innerHTML = '';
        // 兼容旧 id
        var legacy = document.getElementById('ai-log');
        if (legacy) { legacy.textContent = ''; legacy.style.display = 'none'; }
    }

    function classifyStep(msg) {
        var s = String(msg || '');
        if (/错误|Error|failed|失败|HTTP\s*\d/i.test(s)) return 'step-err';
        if (/⚠|警告|Could not|无法|0 笔|0 trades/i.test(s)) return 'step-warn';
        if (/已验证|Verified|Applied|已应用|成功|Connection OK|连接成功/i.test(s)) return 'step-ok';
        if (/AI|建议|思考|回复|Consulting|PARAMS|推理/i.test(s)) return 'step-ai';
        return '';
    }

    function addProcess(msg, kind) {
        var stream = document.getElementById('process-stream');
        var text = String(msg == null ? '' : msg);
        // 同步到隐藏的 legacy log，便于调试
        var legacy = document.getElementById('ai-log');
        if (legacy) legacy.textContent += '> ' + text + '\n';

        if (!stream) return;
        var row = document.createElement('div');
        row.className = 'process-row' + (kind ? ' ' + kind : ' ' + classifyStep(text));
        var now = new Date();
        var hh = String(now.getHours()).padStart(2, '0');
        var mm = String(now.getMinutes()).padStart(2, '0');
        var ss = String(now.getSeconds()).padStart(2, '0');
        row.innerHTML = '<span class="pt">' + hh + ':' + mm + ':' + ss + '</span><span class="pm"></span>';
        row.querySelector('.pm').textContent = text;
        stream.appendChild(row);
        stream.scrollTop = stream.scrollHeight;
    }

    function renderThought(thought) {
        lastAiThought = thought || null;
        var empty = document.getElementById('think-empty');
        var content = document.getElementById('think-content');
        var meta = document.getElementById('think-meta');
        var paramsEl = document.getElementById('think-params');
        var body = document.getElementById('think-body');
        var promptEl = document.getElementById('prompt-preview');
        if (!empty || !content) return;

        if (!thought || !thought.text) {
            empty.style.display = 'block';
            content.style.display = 'none';
            if (promptEl) promptEl.textContent = '';
            return;
        }
        empty.style.display = 'none';
        content.style.display = 'block';
        if (meta) {
            meta.innerHTML =
                '<span>' + escapeHtml(t('thinkModel')) + ': <b>' + escapeHtml(thought.model || '—') + '</b></span>' +
                (thought.goal ? '<span>' + escapeHtml(t('thinkGoal')) + ': <b>' + escapeHtml(thought.goal) + '</b></span>' : '') +
                (thought.provider ? '<span>Provider: <b>' + escapeHtml(thought.provider) + '</b></span>' : '');
        }
        if (paramsEl) {
            if (thought.params) {
                paramsEl.style.display = 'block';
                paramsEl.textContent = t('thinkParams') + ': ' + thought.params;
            } else {
                paramsEl.style.display = 'none';
                paramsEl.textContent = '';
            }
        }
        if (body) body.textContent = thought.text;
        if (promptEl && thought.prompt) promptEl.textContent = thought.prompt;
    }

    function clearThought() {
        renderThought(null);
        var promptEl = document.getElementById('prompt-preview');
        if (promptEl) {
            promptEl.style.display = 'none';
            promptEl.textContent = '';
        }
        var tp = document.getElementById('btn-toggle-prompt');
        if (tp) { delete tp.dataset.open; tp.textContent = t('togglePrompt'); }
    }

    // 提示词折叠
    (function bindPromptToggle() {
        var btn = document.getElementById('btn-toggle-prompt');
        var preview = document.getElementById('prompt-preview');
        if (!btn || !preview) return;
        btn.addEventListener('click', function() {
            var open = btn.dataset.open === '1';
            if (open) {
                preview.style.display = 'none';
                delete btn.dataset.open;
                btn.textContent = t('togglePrompt');
            } else {
                if (!preview.textContent && lastAiThought && lastAiThought.prompt) {
                    preview.textContent = lastAiThought.prompt;
                }
                preview.style.display = preview.textContent ? 'block' : 'none';
                btn.dataset.open = '1';
                btn.textContent = t('hidePrompt');
            }
        });
    })();

    // ── Test AI connection ──
    window.aiTestConnection = function() {
        var btn = document.getElementById('btn-ai-test');
        var url = document.getElementById('ai-url').value;
        var model = document.getElementById('ai-model').value;
        var apiKey = document.getElementById('ai-key').value;

        if (!url) { alert(t('needUrl')); return; }
        if (!model) { alert(t('needModel')); return; }

        btn.disabled = true;
        btn.textContent = '⏳...';

        var provider = document.getElementById('ai-provider').value;
        var isAnthropic = provider === 'claude' || url.includes('anthropic.com');

        var headers, body;
        if (isAnthropic) {
            headers = {
                'x-api-key': apiKey,
                'anthropic-version': '2023-06-01',
                'Content-Type': 'application/json',
                'anthropic-dangerous-direct-browser-access': 'true'
            };
            body = { model: model, max_tokens: 20, messages: [{role:'user',content:'Hi, respond with just "OK"'}] };
        } else {
            headers = { 'Authorization': 'Bearer ' + apiKey, 'Content-Type': 'application/json' };
            body = { model: model, messages: [{role:'user',content:'Hi, respond with just "OK"'}], max_tokens: 20 };
        }

        fetch(url, { method: 'POST', headers: headers, body: JSON.stringify(body) })
        .then(function(r) {
            if (!r.ok) return r.text().then(function(txt) { throw new Error('HTTP ' + r.status + ': ' + txt.substring(0, 200)); });
            return r.json();
        })
        .then(function(d) {
            btn.disabled = false;
            btn.textContent = t('btnTest');
            var reply = '';
            if (isAnthropic && d.content && d.content[0]) {
                reply = d.content[0].text || '';
            } else if (d.choices && d.choices[0]) {
                reply = (d.choices[0].message || {}).content || '';
            }
            alert(t('connOk') + '\nModel: ' + model + '\nResponse: ' + reply.substring(0, 100));
        })
        .catch(function(e) {
            btn.disabled = false;
            btn.textContent = t('btnTest');
            alert(t('connFail') + '\n' + e.message);
        });
    };

    function renderLeaderboard(board, goal) {
        var empty = document.getElementById('think-empty');
        var content = document.getElementById('think-content');
        var meta = document.getElementById('think-meta');
        var paramsEl = document.getElementById('think-params');
        var body = document.getElementById('think-body');
        if (!empty || !content || !body) return;
        if (!board || !board.length) {
            empty.style.display = 'block';
            content.style.display = 'none';
            return;
        }
        empty.style.display = 'none';
        content.style.display = 'block';
        if (meta) {
            meta.innerHTML = '<span>' + escapeHtml(t('leaderboard')) + '</span>' +
                (goal ? '<span>' + escapeHtml(t('thinkGoal')) + ': <b>' + escapeHtml(goal) + '</b></span>' : '');
        }
        if (paramsEl) {
            paramsEl.style.display = 'block';
            paramsEl.textContent = t('thinkParams') + ': ' + (board[0].params || '—');
        }
        var html = '<table style="width:100%;border-collapse:collapse;font-size:11px;">' +
            '<thead><tr style="text-align:left;color:var(--text-muted);">' +
            '<th>#</th><th>Params</th><th>Ret%</th><th>Sharpe</th><th>DD%</th><th>Trades</th></tr></thead><tbody>';
        board.slice(0, 12).forEach(function(row) {
            var ret = Number(row.total_return_pct || 0);
            var cls = ret >= 0 ? 'positive' : 'negative';
            html += '<tr style="border-top:1px solid var(--border);cursor:pointer;" data-params="' +
                escapeHtml(row.params || '') + '">' +
                '<td style="padding:5px 4px;">' + (row.rank || '') + '</td>' +
                '<td style="padding:5px 4px;font-family:var(--font-mono);font-size:10px;max-width:180px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;" title="' +
                escapeHtml(row.params || '') + '">' + escapeHtml(row.params || '') + '</td>' +
                '<td class="' + cls + '" style="padding:5px 4px;">' + ret.toFixed(2) + '</td>' +
                '<td style="padding:5px 4px;">' + Number(row.sharpe_ratio || 0).toFixed(2) + '</td>' +
                '<td style="padding:5px 4px;">' + Number(row.max_drawdown_pct || 0).toFixed(2) + '</td>' +
                '<td style="padding:5px 4px;">' + (row.total_trades || 0) + '</td></tr>';
        });
        html += '</tbody></table>';
        html += '<div style="margin-top:8px;font-size:11px;color:var(--text-sub);">点击某行可将参数填入左侧配置。</div>';
        body.innerHTML = html;
        body.querySelectorAll('tr[data-params]').forEach(function(tr) {
            tr.addEventListener('click', function() {
                var p = tr.getAttribute('data-params');
                if (p) {
                    document.getElementById('bt-params').value = p;
                    saveSettings();
                    addProcess(t('logSuggested') + p, 'step-ai');
                }
            });
        });
        lastAiThought = {
            model: 'local-sweep',
            provider: 'engine',
            goal: goal || '',
            params: board[0].params || null,
            text: body.innerText,
            prompt: null
        };
    }

    // ── Local param sweep (primary path, no external AI) ──
    window.runParamSweep = function() {
        var btn = document.getElementById('btn-sweep-optimize');
        var strategy = document.getElementById('bt-strategy').value;
        var dataFile = document.getElementById('bt-data').value;
        var start = document.getElementById('bt-start').value;
        var end = document.getElementById('bt-end').value;
        var capital = parseFloat(document.getElementById('bt-capital').value) || 125;
        var goal = document.getElementById('ai-goal').value || 'sharpe';
        var mode = (document.getElementById('bt-mode') || {}).value || 'quick';
        var params = document.getElementById('bt-params').value || '';

        if (!dataFile || !start || !end) {
            alert(t('btEmpty') + ' (data/start/end)');
            return;
        }

        btn.disabled = true;
        btn.innerHTML = '<span class="spinner"></span>' + t('running');
        clearProcessStream();
        clearThought();
        setProcessLive(true);
        addProcess(t('sweepStart'));
        addProcess(t('logRange') + dataFile + ' · ' + start + ' → ' + end + ' · mode=' + mode + ' · goal=' + goal);

        fetch('/api/backtest/optimize', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({
                strategy: strategy,
                data_file: dataFile,
                start: start,
                end: end,
                capital: capital,
                goal: goal,
                mode: mode,
                params: params
            })
        })
        .then(function(r) { return r.json(); })
        .then(function(data) {
            btn.disabled = false;
            btn.textContent = t('btnSweep');
            setProcessLive(false);
            if (!data || data.status === 'error') {
                addProcess(t('errPrefix') + (data && (data.message || data.error) || t('btFail')), 'step-err');
                showError((data && (data.message || data.error)) || t('btFail'));
                return;
            }
            addProcess(t('sweepDone') + ': tested=' + (data.tested || 0)
                + ', with_trades=' + (data.with_trades || 0)
                + ', profitable=' + (data.profitable || 0)
                + ', candles=' + (data.candles || 0), 'step-ok');
            if (data.message) addProcess(data.message);

            if (data.leaderboard) renderLeaderboard(data.leaderboard, goal);

            if (data.optimized_params) {
                document.getElementById('bt-params').value = data.optimized_params;
                addProcess(t('logSuggested') + data.optimized_params, 'step-ai');
                saveSettings();
            }

            if (data.optimized) {
                if ((data.profitable || 0) === 0) {
                    addProcess('⚠ ' + t('noProfitable'), 'step-warn');
                }
                var best = data.optimized;
                if (data.baseline) best._baseline = data.baseline;
                best._suggested_params = data.optimized_params;
                lastBacktestResult = best;
                addProcess(t('logOpt') + Number(best.total_return_pct || 0).toFixed(2)
                    + '%, Sharpe=' + Number(best.sharpe_ratio || 0).toFixed(2)
                    + ', Trades=' + (best.total_trades || 0), 'step-ok');
                addProcess(t('logVerified'), 'step-ok');
                renderResults(best);
            } else if (data.baseline) {
                lastBacktestResult = data.baseline;
                renderResults(data.baseline);
            }
        })
        .catch(function(e) {
            btn.disabled = false;
            btn.textContent = t('btnSweep');
            setProcessLive(false);
            addProcess(t('errPrefix') + e.message, 'step-err');
            showError(e.message);
        });
    };

    // ── Run backtest via server API ──
    window.runBacktest = function() {
        var btn = document.getElementById('btn-run-backtest');
        btn.disabled = true;
        btn.innerHTML = '<span class="spinner"></span>' + t('running');
        clearProcessStream();
        setProcessLive(true);
        addProcess(t('runBacktest') + '…');

        var payload = {
            strategy: document.getElementById('bt-strategy').value,
            data_file: document.getElementById('bt-data').value,
            start: document.getElementById('bt-start').value,
            end: document.getElementById('bt-end').value,
            capital: parseFloat(document.getElementById('bt-capital').value),
            params: document.getElementById('bt-params').value || ''
        };
        addProcess(t('logRange') + payload.data_file + ' · ' + payload.start + ' → ' + payload.end);

        fetch('/api/backtest', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify(payload)
        })
        .then(function(r) { return r.json(); })
        .then(function(data) {
            btn.disabled = false;
            btn.textContent = t('runBacktest');
            setProcessLive(false);
            if (data.status === 'error' || data.error || data.message && data.status !== 'ok') {
                addProcess(data.error || data.message || t('btFail'), 'step-err');
                showError(data.error || data.message);
                return;
            }
            addProcess(t('logBase') + Number(data.total_return_pct || 0).toFixed(2)
                + '%, Trades=' + (data.total_trades || 0)
                + ', Candles=' + (data.candles || 0), 'step-ok');
            lastBacktestResult = data;
            renderResults(data);
        })
        .catch(function(e) {
            btn.disabled = false;
            btn.textContent = t('runBacktest');
            setProcessLive(false);
            addProcess(t('reqFail') + e.message, 'step-err');
            showError(t('reqFail') + e.message);
        });
    };

    function showError(msg) {
        document.getElementById('results-empty').style.display = 'none';
        var content = document.getElementById('results-content');
        content.style.display = 'block';
        content.innerHTML = '<div class="result-card"><p class="negative" style="padding:12px">' + escapeHtml(t('errPrefix') + msg) + '</p></div>';
    }

    function renderResults(data) {
        document.getElementById('results-empty').style.display = 'none';
        var content = document.getElementById('results-content');
        content.style.display = 'block';

        var totalReturn = data.total_return_pct || 0;
        var badgeClass = totalReturn >= 0 ? 'badge-profit' : 'badge-loss';
        var badgeText = totalReturn >= 0 ? t('profit') : t('loss');
        var base = data._baseline || null;

        var html = '<div class="result-card">' +
            '<div class="result-header">' +
            '<div class="result-title">' + escapeHtml(t('backtestOn') + (data.strategy || 'grid') + t('onFile') + (data.data_file || '-')) + '</div>' +
            '<span class="result-badge ' + badgeClass + '">' + badgeText + '</span></div>' +
            '<div class="note" style="margin-bottom:12px;"><b>SANDBOX</b> ' + escapeHtml(t('sandboxNote')) + '</div>';

        // 有基线时做对照卡，证明 AI 建议经过同一套回测引擎验证
        if (base) {
            var dRet = Number(totalReturn) - Number(base.total_return_pct || 0);
            var dTr = Number(data.total_trades || 0) - Number(base.total_trades || 0);
            html += '<div class="metrics-grid" style="margin-bottom:10px;">' +
                metric(t('baselineCard') + ' · ' + t('mTotalReturn'), fmtPct(base.total_return_pct || 0), (base.total_return_pct || 0) >= 0) +
                metric(t('optimizedCard') + ' · ' + t('mTotalReturn'), fmtPct(totalReturn), totalReturn >= 0) +
                metric(t('vsBaseline'), (dRet >= 0 ? '+' : '') + dRet.toFixed(2) + '%', dRet >= 0) +
                metric(t('mTrades'), (base.total_trades || 0) + ' → ' + (data.total_trades || 0) + (dTr ? ' (' + (dTr > 0 ? '+' : '') + dTr + ')' : ''), dTr >= 0) +
                '</div>';
        }

        html += '<div class="metrics-grid">' +
            metric(t('mTotalReturn'), fmtPct(totalReturn), totalReturn >= 0) +
            metric(t('mSharpe'), Number(data.sharpe_ratio || 0).toFixed(2), data.sharpe_ratio >= 1) +
            metric(t('mMaxDd'), fmtPct(data.max_drawdown_pct || 0), false) +
            metric(t('mPf'), Number(data.profit_factor || 0).toFixed(2), (data.profit_factor || 0) >= 1) +
            '</div>' +
            '<div class="metrics-grid">' +
            metric(t('mTrades'), data.total_trades || 0, true) +
            metric(t('mFinalEq'), '$' + Number(data.final_capital || 0).toFixed(2), true) +
            metric(t('mAvgWin'), '$' + Number(data.avg_profit || 0).toFixed(2), true) +
            metric(t('mAvgLoss'), '$' + Number(data.avg_loss || 0).toFixed(2), false) +
            '</div>';

        if (data.candles != null) {
            html += '<div style="margin:8px 0 0;font-size:11px;color:var(--text-sub);font-family:var(--font-mono);">Candles=' +
                escapeHtml(String(data.candles)) +
                (data._suggested_params ? ' · PARAMS ' + escapeHtml(data._suggested_params) : '') +
                '</div>';
        }

        var eqCurve = (data.equity_curve || []).map(function(p) { return p.v || p; });
        if (eqCurve.length > 1) {
            html += '<div class="chart-container"><canvas id="bt-chart" class="chart-canvas"></canvas></div>';
        }

        if (data.trades && data.trades.length > 0) {
            html += '<div style="margin-top:12px"><table><thead><tr>' +
                '<th>#</th><th>' + t('thTime') + '</th><th>' + t('thSide') + '</th><th>' + t('thPrice') + '</th><th>' + t('thSize') + '</th><th>' + t('thPnl') + '</th></tr></thead><tbody>';
            var trades = data.trades.slice(-30);
            for (var i = 0; i < trades.length; i++) {
                var tr = trades[i];
                var pnlCls = (tr.pnl || 0) >= 0 ? 'positive' : 'negative';
                html += '<tr><td>' + (i+1) + '</td>' +
                    '<td>' + escapeHtml(tr.timestamp || '-') + '</td>' +
                    '<td>' + escapeHtml(tr.side || '-') + '</td>' +
                    '<td>$' + Number(tr.price||0).toFixed(2) + '</td>' +
                    '<td>' + Number(tr.quantity||0).toFixed(6) + '</td>' +
                    '<td class="' + pnlCls + '">$' + Number(tr.pnl||0).toFixed(2) + '</td></tr>';
            }
            html += '</tbody></table></div>';
        }

        html += '</div>';

        // "Apply to Live" button — sends current params to live strategy
        var currentParams = document.getElementById('bt-params').value;
        var currentStrategy = document.getElementById('bt-strategy').value;
        if (currentParams) {
            html += '<div style="margin-top:16px;display:flex;gap:8px;align-items:center">' +
                '<button class="btn btn-ai" id="btn-apply-live" style="flex:1" onclick="applyToLive()">' + t('applyLive') + '</button>' +
                '<div style="font-size:11px;color:var(--text-sub);flex:1">' + t('applyLiveHint') + ' <b>' + escapeHtml(currentStrategy) + '</b> ' + t('applyLiveHint2') + ' <code>' + escapeHtml(currentParams) + '</code> ' + t('applyLiveHint3') + '</div>' +
                '</div>';
        }

        content.innerHTML = html;

        if (eqCurve.length > 1) {
            setTimeout(function() { drawChart('bt-chart', eqCurve); }, 50);
        }
    }

    // Apply optimized params to live trading
    window.applyToLive = function() {
        var btn = document.getElementById('btn-apply-live');
        var strategy = document.getElementById('bt-strategy').value;
        var paramsStr = document.getElementById('bt-params').value;
        if (!paramsStr) { alert(t('noParams')); return; }

        // Parse params string into object
        var params = {};
        paramsStr.split(',').forEach(function(pair) {
            var kv = pair.split('=');
            if (kv.length === 2) {
                var val = parseFloat(kv[1].trim());
                params[kv[0].trim()] = isNaN(val) ? kv[1].trim() : val;
            }
        });

        // Map strategy names
        var strategyName = strategy === 'grid' ? 'grid_trading' : strategy === 'trend' ? 'trend_following' : strategy;

        btn.disabled = true;
        btn.textContent = '⏳ ' + t('applying');

        fetch('/api/strategy', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ strategy: strategyName, params: params })
        })
        .then(function(r) { return r.json(); })
        .then(function(d) {
            btn.disabled = false;
            btn.textContent = t('applied');
            btn.style.background = 'var(--success)';
            addProcess(t('applied') + ': ' + strategyName + ' · ' + paramsStr, 'step-ok');
            setTimeout(function() {
                btn.textContent = t('applyLive');
                btn.style.background = '';
            }, 3000);
        })
        .catch(function(e) {
            btn.disabled = false;
            btn.textContent = t('applyFail');
            setTimeout(function() { btn.textContent = t('applyLive'); }, 3000);
            alert(t('failApply') + e.message);
        });
    };

    function metric(label, value, isGood) {
        var cls = isGood ? 'positive' : 'negative';
        return '<div class="metric"><div class="metric-value ' + cls + '">' + value + '</div>' +
            '<div class="metric-label">' + label + '</div></div>';
    }

    function fmtPct(v) { return (v >= 0 ? '+' : '') + v.toFixed(2) + '%'; }

    function drawChart(canvasId, equityCurve) {
        var canvas = document.getElementById(canvasId);
        if (!canvas || equityCurve.length < 2) return;
        var ctx = canvas.getContext('2d');
        var dpr = window.devicePixelRatio || 1;
        var rect = canvas.getBoundingClientRect();
        canvas.width = rect.width * dpr;
        canvas.height = rect.height * dpr;
        ctx.scale(dpr, dpr);
        var w = rect.width, h = rect.height;
        ctx.clearRect(0, 0, w, h);

        var vals = equityCurve;
        var minV = Math.min.apply(null, vals) * 0.998;
        var maxV = Math.max.apply(null, vals) * 1.002;
        var range = maxV - minV || 1;

        var styles = getComputedStyle(document.documentElement);
        var css = function(name, fallback) { return styles.getPropertyValue(name).trim() || fallback; };
        ctx.strokeStyle = css('--border', '#D6D1C4');
        ctx.lineWidth = 1;
        for (var g = 0; g < 4; g++) {
            var gy = h * 0.05 + (h * 0.9 / 3) * g;
            ctx.beginPath(); ctx.moveTo(0, gy); ctx.lineTo(w, gy); ctx.stroke();
        }

        var isProfit = vals[vals.length - 1] >= vals[0];
        var lineColor = isProfit ? css('--success', '#2F5D3A') : css('--danger', '#A14A3F');
        var fillColor = isProfit ? css('--success-bg', 'rgba(47,93,58,0.12)') : css('--danger-bg', 'rgba(161,74,63,0.12)');

        ctx.beginPath();
        ctx.strokeStyle = lineColor;
        ctx.lineWidth = 2;
        for (var i = 0; i < vals.length; i++) {
            var x = (i / (vals.length - 1)) * w;
            var y = h * 0.05 + (1 - (vals[i] - minV) / range) * h * 0.9;
            if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
        }
        ctx.stroke();
        ctx.lineTo(w, h); ctx.lineTo(0, h); ctx.closePath();
        ctx.fillStyle = fillColor;
        ctx.fill();

        ctx.fillStyle = css('--text-muted', '#7A766C');
        ctx.font = '10px ui-monospace, monospace';
        ctx.textAlign = 'right';
        ctx.fillText('$' + maxV.toFixed(0), w - 4, h * 0.05 + 12);
        ctx.fillText('$' + minV.toFixed(0), w - 4, h * 0.95);
    }

    // ── helpers for AI-driven multi-round backtest ──
    function runServerBacktest(payload) {
        return fetch('/api/backtest', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(payload)
        }).then(function(r) { return r.json(); }).then(function(result) {
            if (!result || result.status === 'error' || result.error) {
                throw new Error((result && (result.message || result.error)) || t('btFail'));
            }
            if (result.total_return_pct === undefined && result.total_trades === undefined) {
                throw new Error(t('btEmpty'));
            }
            return result;
        });
    }

    function scoreResult(goal, r) {
        if (!r || !r.total_trades) return -1e18;
        var ret = Number(r.total_return_pct || 0);
        var sharpe = Number(r.sharpe_ratio || 0);
        var dd = Number(r.max_drawdown_pct || 0);
        if (goal === 'return') return ret;
        if (goal === 'drawdown') return -dd;
        if (goal === 'balanced') return ret - dd * 0.5 + sharpe * 2;
        return sharpe;
    }

    function summarizeTrial(trial) {
        return 'params=' + trial.params
            + ' | return=' + Number(trial.total_return_pct || 0).toFixed(2) + '%'
            + ' | sharpe=' + Number(trial.sharpe_ratio || 0).toFixed(2)
            + ' | maxDD=' + Number(trial.max_drawdown_pct || 0).toFixed(2) + '%'
            + ' | trades=' + (trial.total_trades || 0)
            + ' | winRate=' + Number(trial.win_rate_pct || 0).toFixed(1) + '%';
    }

    function paramSpaceHint(strategy, capital) {
        var cap = Number(capital) || 125;
        if (strategy === 'trend' || strategy === 'trend_following') {
            var nHi = Math.max(10, Math.floor(cap * 0.9));
            return 'Strategy=trend_following. Allowed params:\n'
                + '- fast_ma (int 5-21)\n- slow_ma (int 14-80, must be > fast_ma)\n'
                + '- stop_loss (float 0.02-0.08)\n- take_profit (float 0.04-0.15, > stop_loss)\n'
                + '- trailing_stop (float 0-0.05, 0=off)\n'
                + '- notional (USD ' + Math.floor(cap * 0.2) + '-' + nHi + ', MUST be < capital ' + cap + ')\n'
                + 'Example: PARAMS: fast_ma=10,slow_ma=30,stop_loss=0.04,take_profit=0.08,trailing_stop=0,notional='
                + Math.floor(cap * 0.5);
        }
        if (strategy === 'dca') {
            return 'Strategy=dca. Allowed params:\n'
                + '- interval (hours 1-24)\n- amount (USD 1-' + Math.floor(cap * 0.5) + ')\n'
                + '- dip_threshold (percent 0-10)\n'
                + 'Example: PARAMS: interval=4,amount=10,dip_threshold=2';
        }
        return 'Strategy=grid_trading. Allowed params:\n'
            + '- grid_count (int 4-20)\n- investment (USD per grid 3-' + Math.floor(Math.min(80, cap * 0.4)) + ')\n'
            + '- deviation (float 0.003-0.03)\n'
            + 'Example: PARAMS: grid_count=10,investment=15,deviation=0.01';
    }

    function buildAgentPrompt(ctx) {
        var lines = [];
        lines.push('You are a quant researcher driving iterative strategy backtests.');
        lines.push('You do NOT invent performance numbers. Only propose parameters.');
        lines.push('Every candidate will be executed by a real backtest engine; next messages include verified results.');
        lines.push('');
        lines.push('Context:');
        lines.push('- Market data: ' + ctx.data_file + ' from ' + ctx.start + ' to ' + ctx.end);
        lines.push('- Candles: ' + (ctx.candles || '?'));
        lines.push('- Capital: $' + ctx.capital);
        lines.push('- Goal: ' + ctx.goalText);
        lines.push('- Round: ' + ctx.round + ' / ' + ctx.rounds);
        lines.push('');
        lines.push(paramSpaceHint(ctx.strategy, ctx.capital));
        lines.push('');
        lines.push('Verified trials so far (newest last):');
        ctx.trials.forEach(function(tr, i) {
            lines.push((i + 1) + ') ' + summarizeTrial(tr));
        });
        lines.push('');
        lines.push('Task: propose exactly ' + ctx.candidates + ' NEW parameter sets that are likely better for the goal.');
        lines.push('Do not repeat params already tried. Prefer diversity (explore) then exploit winners.');
        lines.push('If many trials have 0 trades, fix sizing/notional/deviation so the strategy can trade.');
        lines.push('Respond with ' + ctx.candidates + ' lines in this EXACT form (and optional brief reasoning after):');
        lines.push('PARAMS: key=value,key=value,...');
        return lines.join('\n');
    }

    function parseAllParams(text) {
        var out = [];
        var seen = {};
        String(text || '').split(/\r?\n/).forEach(function(line) {
            var m = line.match(/PARAMS:\s*(.+)/i);
            if (!m) return;
            var raw = m[1].trim().replace(/[.;]+$/, '');
            // keep only key=value pairs
            var cleaned = raw.split(',').map(function(p) { return p.trim(); })
                .filter(function(p) { return /^\w+\s*=\s*[-+]?[\d.]+$/.test(p) || /^\w+\s*=\s*[\w.+-]+$/.test(p); })
                .join(',');
            if (!cleaned) return;
            var key = cleaned.replace(/\s+/g, '');
            if (seen[key]) return;
            seen[key] = true;
            out.push(cleaned);
        });
        if (!out.length) {
            var one = parseSuggestedParams(text);
            if (one) out.push(one);
        }
        return out;
    }

    // ── AI-driven multi-round backtest (PRIMARY path uses user API key) ──
    window.aiOptimize = function() {
        var apiKey = document.getElementById('ai-key').value;
        var apiUrl = document.getElementById('ai-url').value;
        var modelId = document.getElementById('ai-model').value;
        var maxTokens = readMaxTokensFromUi();
        var rounds = Math.max(1, Math.min(5, parseInt((document.getElementById('ai-rounds') || {}).value, 10) || 3));
        var candidatesN = Math.max(1, Math.min(5, parseInt((document.getElementById('ai-candidates') || {}).value, 10) || 3));

        if (!apiUrl) { alert(t('needUrl')); return; }
        if (!modelId) { alert(t('needModel')); return; }
        if (!apiKey && document.getElementById('ai-provider').value !== 'ollama') {
            alert(t('needKey')); return;
        }

        var btn = document.getElementById('btn-ai-optimize');
        btn.disabled = true;
        btn.innerHTML = '<span class="spinner"></span>' + t('analyzing');
        clearProcessStream();
        clearThought();
        setProcessLive(true);

        var provider = document.getElementById('ai-provider').value;
        var isAnthropic = provider === 'claude' || apiUrl.includes('anthropic.com');
        var goal = document.getElementById('ai-goal').value;
        var goalText = { sharpe: 'Maximize Sharpe', 'return': 'Maximize return', drawdown: 'Minimize max drawdown', balanced: 'Balance return & risk' }[goal] || goal;
        var strategy = document.getElementById('bt-strategy').value;
        var capital = parseFloat(document.getElementById('bt-capital').value) || 125;
        var baseParams = document.getElementById('bt-params').value || '';
        if (!baseParams) {
            if (strategy === 'trend' || strategy === 'trend_following') {
                baseParams = 'fast_ma=14,slow_ma=50,stop_loss=0.05,take_profit=0.06,trailing_stop=0,notional=' + Math.max(10, capital * 0.5).toFixed(2);
            } else if (strategy === 'dca') {
                baseParams = 'interval=4,amount=10,dip_threshold=2';
            } else {
                baseParams = 'grid_count=10,investment=8,deviation=0.012';
            }
            document.getElementById('bt-params').value = baseParams;
        }

        var payloadBase = {
            strategy: strategy,
            data_file: document.getElementById('bt-data').value,
            start: document.getElementById('bt-start').value,
            end: document.getElementById('bt-end').value,
            capital: capital
        };

        addProcess(t('aiLoopStart'), 'step-ai');
        addProcess('Provider: ' + provider + ' | Model: ' + modelId + ' | rounds=' + rounds + ' | candidates=' + candidatesN);
        addProcess(t('logRange') + payloadBase.data_file + ' · ' + payloadBase.start + ' → ' + payloadBase.end);
        addProcess(t('logBaseline') + baseParams);

        var trials = []; // {params, metrics..., score, source}
        var thoughtLog = [];
        var tried = {};

        function pushTrial(params, result, source) {
            var row = {
                params: params,
                total_return_pct: result.total_return_pct,
                sharpe_ratio: result.sharpe_ratio,
                max_drawdown_pct: result.max_drawdown_pct,
                total_trades: result.total_trades,
                win_rate_pct: result.win_rate_pct,
                profit_factor: result.profit_factor,
                score: scoreResult(goal, result),
                source: source || 'ai',
                result: result
            };
            trials.push(row);
            tried[params.replace(/\s+/g, '')] = true;
            return row;
        }

        function bestTrial() {
            if (!trials.length) return null;
            return trials.slice().sort(function(a, b) { return b.score - a.score; })[0];
        }

        function refreshBoard() {
            var board = trials.slice()
                .sort(function(a, b) { return b.score - a.score; })
                .map(function(tr, i) {
                    return {
                        rank: i + 1,
                        params: tr.params,
                        total_return_pct: tr.total_return_pct,
                        sharpe_ratio: tr.sharpe_ratio,
                        max_drawdown_pct: tr.max_drawdown_pct,
                        total_trades: tr.total_trades,
                        win_rate_pct: tr.win_rate_pct,
                        profit_factor: tr.profit_factor,
                        score: tr.score
                    };
                });
            renderLeaderboard(board, goalText);
            var b = bestTrial();
            if (b) {
                renderThought({
                    model: modelId,
                    provider: provider,
                    goal: goalText,
                    params: b.params,
                    text: thoughtLog.join('\n\n——\n\n'),
                    prompt: lastPrompt || ''
                });
            }
        }

        var lastPrompt = '';
        var baselineResult = null;

        // Round 0: baseline backtest
        runServerBacktest(Object.assign({}, payloadBase, { params: baseParams }))
        .then(function(baseResult) {
            baselineResult = baseResult;
            pushTrial(baseParams, baseResult, 'baseline');
            addProcess(t('logBase') + Number(baseResult.total_return_pct || 0).toFixed(2)
                + '%, Sharpe=' + Number(baseResult.sharpe_ratio || 0).toFixed(2)
                + ', Trades=' + (baseResult.total_trades || 0)
                + ', Candles=' + (baseResult.candles || 0), 'step-ok');
            if ((baseResult.total_trades || 0) === 0) addProcess('⚠ ' + t('logNoTrades'), 'step-warn');
            refreshBoard();

            // Sequential AI rounds
            var chain = Promise.resolve();
            for (var r = 1; r <= rounds; r++) {
                (function(round) {
                    chain = chain.then(function() {
                        addProcess(t('aiRound') + round + '/' + rounds + ' — ' + t('aiPropose'), 'step-ai');
                        var prompt = buildAgentPrompt({
                            strategy: strategy,
                            data_file: payloadBase.data_file,
                            start: payloadBase.start,
                            end: payloadBase.end,
                            candles: baselineResult.candles,
                            capital: capital,
                            goalText: goalText,
                            round: round,
                            rounds: rounds,
                            candidates: candidatesN,
                            trials: trials
                        });
                        lastPrompt = prompt;
                        thoughtLog.push('### Round ' + round + ' prompt prepared\n' + prompt.slice(0, 500) + (prompt.length > 500 ? '…' : ''));
                        refreshBoard();

                        return callAI(apiUrl, modelId, apiKey, prompt, maxTokens, isAnthropic)
                        .then(function(suggestion) {
                            addProcess(t('logAiResp') + ' (round ' + round + ')', 'step-ai');
                            thoughtLog.push('### Round ' + round + ' model reply\n' + suggestion);
                            refreshBoard();

                            var props = parseAllParams(suggestion).filter(function(p) {
                                return !tried[p.replace(/\s+/g, '')];
                            }).slice(0, candidatesN);

                            if (!props.length) {
                                addProcess('⚠ ' + t('aiNoCandidate'), 'step-warn');
                                return;
                            }

                            // verify each candidate sequentially
                            var v = Promise.resolve();
                            props.forEach(function(p, idx) {
                                v = v.then(function() {
                                    addProcess(t('aiVerify') + ' [' + (idx + 1) + '/' + props.length + '] ' + p);
                                    return runServerBacktest(Object.assign({}, payloadBase, { params: p }))
                                    .then(function(res) {
                                        var row = pushTrial(p, res, 'ai-r' + round);
                                        addProcess(
                                            '✓ ' + p + ' → ret=' + Number(res.total_return_pct || 0).toFixed(2)
                                            + '% trades=' + (res.total_trades || 0)
                                            + ' sharpe=' + Number(res.sharpe_ratio || 0).toFixed(2),
                                            (res.total_trades || 0) > 0 ? 'step-ok' : 'step-warn'
                                        );
                                        refreshBoard();
                                        return row;
                                    })
                                    .catch(function(err) {
                                        addProcess('✗ ' + p + ' — ' + err.message, 'step-err');
                                    });
                                });
                            });
                            return v;
                        });
                    });
                })(r);
            }
            return chain;
        })
        .then(function() {
            btn.disabled = false;
            btn.textContent = t('btnOptimize');
            setProcessLive(false);

            var best = bestTrial();
            if (!best) {
                showError(t('btEmpty'));
                return;
            }
            document.getElementById('bt-params').value = best.params;
            saveSettings();
            addProcess(t('aiBest') + ': ' + best.params
                + ' | ret=' + Number(best.total_return_pct || 0).toFixed(2) + '%'
                + ' trades=' + (best.total_trades || 0), 'step-ok');
            addProcess(t('logVerified'), 'step-ok');

            var out = best.result || best;
            if (baselineResult) out._baseline = baselineResult;
            out._suggested_params = best.params;
            lastBacktestResult = out;
            refreshBoard();
            renderResults(out);
        })
        .catch(function(e) {
            btn.disabled = false;
            btn.textContent = t('btnOptimize');
            setProcessLive(false);
            addProcess(t('errPrefix') + e.message, 'step-err');
            showError(e.message);
        });
    };

    function callAI(url, model, apiKey, prompt, maxTokens, isAnthropic) {
        var headers, body;
        // 二次钳制，防止调用方漏夹或传入异常
        var tokens = clampMaxTokens(maxTokens);

        if (isAnthropic) {
            headers = {
                'x-api-key': apiKey,
                'anthropic-version': '2023-06-01',
                'Content-Type': 'application/json',
                'anthropic-dangerous-direct-browser-access': 'true'
            };
            body = { model: model, max_tokens: tokens, messages: [{role:'user',content:prompt}] };
        } else {
            headers = { 'Content-Type': 'application/json' };
            if (apiKey) headers['Authorization'] = 'Bearer ' + apiKey;
            body = { model: model, messages: [{role:'user',content:prompt}], max_tokens: tokens };
        }

        return fetch(url, { method: 'POST', headers: headers, body: JSON.stringify(body) })
            .then(function(r) {
                if (!r.ok) return r.text().then(function(t) { throw new Error('HTTP ' + r.status + ': ' + t.substring(0, 300)); });
                return r.json();
            })
            .then(function(d) {
                // Anthropic format
                if (isAnthropic && d.content && d.content[0]) {
                    return d.content[0].text || '';
                }
                // OpenAI-compatible format
                if (d.choices && d.choices[0] && d.choices[0].message) {
                    return d.choices[0].message.content;
                }
                throw new Error('Unexpected response format: ' + JSON.stringify(d).substring(0, 300));
            });
    }

    function parseSuggestedParams(text) {
        var match = text.match(/PARAMS:\s*([\w=.,]+)/i);
        if (match) return match[1].trim();
        var gc = text.match(/grid_count\s*=\s*(\d+)/);
        var inv = text.match(/investment\s*=\s*([\d.]+)/);
        var dev = text.match(/deviation\s*=\s*([\d.]+)/);
        if (gc && inv && dev) {
            return 'grid_count=' + gc[1] + ',investment=' + inv[1] + ',deviation=' + dev[1];
        }
        return null;
    }

    window.runOpenCodeBacktest = function() {
        var btn = document.getElementById('btn-opencode-optimize');
        var model = (document.getElementById('opencode-model').value || '').trim();
        if (!model) {
            alert(t('needOcModel'));
            return;
        }

        btn.disabled = true;
        btn.innerHTML = '<span class="spinner"></span>' + t('running');
        clearProcessStream();
        clearThought();
        setProcessLive(true);
        addProcess(t('ocStart'));
        addProcess('Model: ' + model);

        fetch('/api/backtest/opencode-optimize', {
            method: 'POST',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify({
                strategy: document.getElementById('bt-strategy').value,
                data_file: document.getElementById('bt-data').value,
                start: document.getElementById('bt-start').value,
                end: document.getElementById('bt-end').value,
                capital: parseFloat(document.getElementById('bt-capital').value),
                params: document.getElementById('bt-params').value || '',
                goal: document.getElementById('ai-goal').value,
                opencode_model: model
            })
        })
        .then(function(r) { return r.json(); })
        .then(function(data) {
            btn.disabled = false;
            btn.textContent = t('btnOpencode');
            setProcessLive(false);
            if (data.status !== 'ok') {
                addProcess(t('errPrefix') + (data.message || data.error || t('ocFail')), 'step-err');
                showError(data.message || data.error || t('ocFail'));
                return;
            }
            if (data.suggestion) {
                renderThought({
                    model: model,
                    provider: 'opencode',
                    goal: document.getElementById('ai-goal').value,
                    params: data.optimized_params || null,
                    text: data.suggestion.trim(),
                    prompt: null
                });
                addProcess(t('logAiResp'), 'step-ai');
            }
            if (data.optimized_params) {
                document.getElementById('bt-params').value = data.optimized_params;
                addProcess(t('logSuggested') + data.optimized_params, 'step-ok');
            }
            lastBacktestResult = data.optimized || data.base || data;
            if (data.base && data.optimized) {
                lastBacktestResult._baseline = data.base;
                lastBacktestResult._suggested_params = data.optimized_params;
            }
            renderResults(lastBacktestResult);
            addProcess(t('logVerified'), 'step-ok');
        })
        .catch(function(e) {
            btn.disabled = false;
            btn.textContent = t('btnOpencode');
            setProcessLive(false);
            addProcess(t('errPrefix') + e.message, 'step-err');
            showError(t('ocReqFail') + e.message);
        });
    };

    // 绑定语言切换，并在启动时应用（与主面板 lighter-lang 同步）
    var langBtn = document.getElementById('ai-lang-btn');
    if (langBtn) langBtn.addEventListener('click', toggleLang);
    applyI18n();

})();
