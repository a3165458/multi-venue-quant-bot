// Multi-Venue Quant Bot — Dashboard Logic
(function() {
    'use strict';

    // ── Config ──
    const MAX_EQUITY_PTS = 5000;
    const EQUITY_THROTTLE = 15000;
    let ws = null;
    let reconnTimer = null;
    let wsConnected = false;
    let wsEverOpened = false;
    let activePage = 'dashboard';
    let equityData = [];
    let allTrades = [];
    let equityChart = null;
    let revenueChart = null;
    let notifications = [];
    let ordersData = [];
    let equityRange = 'all';
    let lastMakerParams = null;
    let lastStrategyName = 'maker_quote';

    const $ = id => document.getElementById(id);
    const fmtPnl = v => (v >= 0 ? '+$' : '-$') + Math.abs(v).toFixed(2);
    const fmtPct = v => (v >= 0 ? '+' : '') + v.toFixed(2) + '%';
    const pnlArrow = v => v > 0.001 ? '▲ ' : v < -0.001 ? '▼ ' : '— ';
    const pnlClass = v => v >= 0 ? 'c-up' : 'c-down';
    const escapeHtml = value => String(value ?? '').replace(/[&<>"']/g, char => ({
        '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
    })[char]);
    const tradeAction = t => t.action || t.close_type || t.trade_type || 'Order';
    const isCloseAction = action => /Close|Stop|Emergency|Liquidat/i.test(action || '');
    const isTerminalCloseAction = action => /Full Close|Stop|Emergency|Liquidat/i.test(action || '');
    // Hyperliquid fills are all action="Fill" but carry net realized pnl
    // (closedPnl - fee); treat them as pnl-bearing rows for the stats pages.
    const hasTradePnl = t => typeof t.pnl === 'number' && t.pnl !== 0;

    // ── i18n ──
    const i18nStrings = {
        en: {
            dashboard: 'Dashboard', strategies: 'Strategies', portfolio: 'Portfolio',
            history: 'History', settings: 'Settings', equity: 'Equity',
            unrealizedPnl: 'Unrealized P&L', dailyPnl: 'Daily P&L', totalPnl: 'Total P&L',
            equityCurve: 'Equity Curve', weeklyPnl: 'Weekly P&L',
            positions: 'Positions', orders: 'Orders', trades: 'Trades', log: 'Log',
            orderBook: 'Order Book', riskMonitor: 'Risk Monitor',
            maxDrawdown: 'Max Drawdown', dailyLoss: 'Daily Loss',
            available: 'Available', peakEquity: 'Peak Equity', openOrders: 'Open Orders',
            dailyPnlHistory: 'Daily P&L History',
            tradingControls: 'Trading Controls', manageMarkets: 'Manage markets and trading state',
            activeMarkets: 'Active Markets', quickActions: 'Quick Actions',
            saveMarketConfig: 'Save Market Config', pauseTrading: 'Pause Trading',
            resumeTrading: 'Resume Trading', cancelAllOrders: 'Cancel All Orders',
            performance: 'Performance', winRate: 'Win Rate', totalTrades: 'Total Trades',
            maxLeverage: 'Max Leverage',
            investPerGrid: 'Investment per Grid ($)', priceDeviation: 'Price Deviation (%)',
            dcaSub: 'Dollar-Cost Averaging', trendSub: 'EMA Crossover + RSI',
            buyInterval: 'Buy Interval (hours)', amountPerBuy: 'Amount per Buy ($)',
            dipThreshold: 'Dip Threshold (%)', fastEma: 'Fast EMA Period',
            slowEma: 'Slow EMA Period', rsiPeriod: 'RSI Period',
            tradeHistory: 'Trade History', fullAuditTrail: 'Full audit trail of all executed trades',
            exportCsv: 'Export CSV', volume: 'Volume', avgDuration: 'Avg Duration',
            systemStatus: 'System Status', botStatus: 'Bot Status',
            apiConnection: 'API Connection', riskLimits: 'Risk Limits',
            theme: 'Theme', themeDesc: 'Switch between light and dark mode for comfortable viewing.',
            toggleTheme: 'Toggle Theme', market: 'Market', side: 'Side', size: 'Size',
            entry: 'Entry', mark: 'Mark', price: 'Price', qty: 'Qty', filled: 'Filled',
            status: 'Status', time: 'Time', asset: 'Asset', pnl: 'PNL',
            noPositions: 'No open positions', noOrders: 'No open orders',
            noTrades: 'No trades yet', noHistory: 'No trade history yet',
            searchPlaceholder: 'Search...', searchByAsset: 'Search by asset, side...',
            connecting: 'Connecting...', liveTrading: 'Live Trading', disconnected: 'Disconnected',
            connectionLost: 'Connection lost. Reconnecting...',
            activateStrategy: 'Activate Strategy', stopLoss: 'Stop Loss (%)', takeProfit: 'Take Profit (%)',
            loopKicker: 'Two control loops · WS main + 10s refresh', theLoop: 'The Loop',
            ordersKicker: 'Every cell is a resting order · live from /ws',
            totalReturn: 'Total return · since inception', lastPush: 'Last push',
            ordersPlaced: 'Orders placed', fillsCapped: 'Fills (last 200)',
            legendPush: 'on every /ws state frame', legendRisk: 'drawdown / daily-loss figures change',
            legendOrder: 'open-order count changes', legendFill: 'trade_history gains a record',
            drawdownLimits: 'Drawdown limits', refreshLoop: 'Live · 10s refresh loop',
            legendDot: 'one lap = one /ws frame (3s)', tapeIdle: 'Waiting for position data', sinceFill: 'Since last fill', railEmpty: 'No events this session',
            brandKicker: 'WS in \u00b7 orders out \u00b7 two control loops', today: 'today',
            initial: 'Initial', headroom: 'Headroom', headroomCap: 'Lowest remaining margin',
            riskKicker: 'How much room is left',
            equityFootL: 'Every point is a /api/pnl snapshot', equityFootR: 'Range set on the return card',
            swarmFootL: 'Nearest to market is inverted', swarmFootR: 'Fill bar = filled / quantity',
            railKicker: 'Real events only \u00b7 newest first',
            envKicker: 'Read at process start \u00b7 restart required', envTitle: 'Environment',
            networkKicker: 'Active connection \u00b7 restart to switch', networkTitle: 'Network',
            networkCredentials: 'Each venue uses an isolated credential profile. Aster V3 uses a sub-account user address and an API Wallet signer.',
            saveNetwork: 'Use selected network after restart', networkFootL: 'Current connection stays unchanged', networkFootR: 'Selection is saved to .env',
            envWarnTag: 'Security',
            envWarn: 'Mutation endpoints require the per-process dashboard credential. Keep this service on a trusted network. The secret key is write-only \u2014 it is never sent back to the browser.',
            writeOnly: '(write-only)', currentValue: 'current',
            secretHint: 'Use exchange-issued API credentials, never a wallet L1 key. A blank secret keeps the existing value.',
            saveEnv: 'Save network credentials', envFootL: 'One file · isolated network prefixes', envFootR: 'Restart the bot to apply', eventLog: 'Event Log', thisWeek: 'This week \u00b7 realised',
            confirmCancel: 'Cancel ALL open orders? This cannot be undone.', navMenu: 'Menu',
            makerSub: 'Two-sided ALO when flat · flatten-only after fill, IOC after timeout',
            quoteNotional: 'Quote notional ($)', requoteCooldown: 'Replace cooldown (s)',
            joinInside: 'Join-inside ticks', flattenTimers: 'Flatten mid / IOC (s)',
            flattenOnly: 'Flatten-only after fill', applyMaker: 'Apply maker quote',
            noMarkets: 'No markets from this venue yet',
            mq_quote_mode: 'Quote mode',
            mq_per_quote_notional: 'Quote notional ($)',
            mq_min_quote_notional: 'Min quote notional ($)',
            mq_total_quote_budget: 'Total quote budget ($)',
            mq_soft_cap_notional: 'Inventory soft cap ($)',
            mq_hard_cap_notional: 'Inventory hard cap ($)',
            mq_spread_bps: 'Spread (bps, mid_spread mode)',
            mq_requote_threshold_bps: 'Requote threshold (bps)',
            mq_requote_cooldown_secs: 'Replace cooldown (s)',
            mq_join_inside_ticks: 'Join-inside ticks',
            mq_ema_period: 'Trend EMA period',
            mq_trend_block_bps: 'Trend block (bps)',
            mq_max_skew_bps: 'Max inventory skew (bps)',
            mq_vol_window: 'Vol window (bars, 0=off)',
            mq_vol_multiplier: 'Vol spread multiplier',
            mq_min_book_spread_bps: 'Min book spread (bps)',
            mq_max_book_spread_bps: 'Max book spread (bps)',
            mq_wide_book_size_mult: 'Wide-book size multiplier',
            mq_max_bbo_imbalance: 'Max BBO imbalance (x, 0=off)',
            mq_jump_circuit_breaker_bps: 'Jump breaker (bps)',
            mq_circuit_breaker_cooldown_secs: 'Breaker cooldown (s)',
            mq_feature_interval_secs: 'Feature interval (s)',
            mq_flatten_mid_secs: 'Flatten mid after (s)',
            mq_flatten_ioc_secs: 'IOC flatten after (s)',
            mq_cash_open_guard_before_minutes: 'Cash-open guard before (min)',
            mq_cash_open_guard_after_minutes: 'Cash-open guard after (min)',
            mq_trend_filter: 'Trend filter',
            mq_cash_open_guard: 'Cash-open guard',
            mq_flatten_only: 'Flatten-only after fill (then IOC)',
            mqJoinBest: 'Join best bid/ask',
            mqMidSpread: 'Mid-spread',
            activeBadge: '● Active',
            inactiveBadge: '○ Inactive',
            pausedBadge: '⏸ Paused',
            applying: 'Applying...',
            applyFailed: 'Failed to apply',
            applyMakerOk: 'Maker quote applied',
            makerActivated: 'Maker quote activated',
            gridActivated: 'Grid strategy activated',
            dcaActivated: 'DCA strategy activated',
            trendActivated: 'Trend following strategy activated',
            justNow: 'just now',
            minutesAgo: 'm ago',
            hoursAgo: 'h ago',
            daysAgo: 'd ago',
            noNotifs: 'No notifications yet',
            notifications: 'Notifications',
            clearAll: 'Clear all',
            rangeError: 'must be between',
            softCapError: 'soft cap must not exceed hard cap',
            minQuoteError: 'min quote notional must not exceed quote notional',
            marketsUpdated: 'Markets updated',
            marketsFailed: 'Failed to update markets',
            tradingPausedMsg: 'Trading paused',
            tradingResumedMsg: 'Trading resumed',
            actionFailed: 'Failed',
            allCancelled: 'All orders cancelled',
            cancelFailed: 'Failed to cancel orders',
            riskSaved: 'Risk settings saved successfully',
            riskUpdated: 'Risk settings updated',
            networkError: 'Network error',
            wsConnected: 'WebSocket connected',
            wsDisconnected: 'WebSocket disconnected, reconnecting...',
            initLog: 'Dashboard initializing...',
            failedPnl: 'Failed to load PnL data',
            failedStrategy: 'Failed to load strategy config',
            noMatching: 'No matching trades',
            noClosed: 'No closed trades yet',
            noDaily: 'No daily data yet',
            noData: 'No data',
            totalWord: 'Total',
            closedTrades: 'Closed Trades',
            avgHold: 'Avg Hold',
            posSummary: 'Position P&L Summary',
            gridStrategy: 'Grid Strategy',
            configuration: 'Configuration',
            gridCount: 'Grid Count',
            live: 'Live',
            riskSlTp: 'Risk: SL / TP',
            maxOpenOrders: 'Max Open Orders',
            dcaStrategy: 'DCA Strategy',
            trendFollowing: 'Trend Following',
            makerQuote: 'Maker Quote',
            loading: 'Loading...',
            loadingMarkets: 'Loading markets…',
            ompAgent: 'OMP Agent',
            idle: 'IDLE',
            shadowKicker: 'Virtual quotes only · risk exits remain armed',
            asterShadow: 'Aster shadow maker',
            runtime: 'Runtime',
            bboEvents: 'BBO events',
            eventLag: 'Event lag',
            depthLag: 'Depth lag',
            strategyEval: 'Strategy eval',
            queueAhead: 'Queue ahead',
            quoteReqMin: 'Quote requests/min',
            amendSavings: 'Amend savings',
            virtualFills: 'Virtual fills',
            virtualVolume: 'Virtual volume',
            volumeHour: 'Volume/hour',
            hftKicker: 'Multi-profile · near-BBO · no real orders',
            hftLab: 'HFT shadow lab',
            profile: 'Profile',
            offset: 'Offset',
            cooldown: 'Cooldown',
            reqMin: 'Req/min',
            amendSave: 'Amend save',
            fills: 'Fills',
            volHour: 'Volume/h',
            markout1s: '1s markout',
            markout5s: '5s markout',
            markout30s: '30s markout',
            collectingHft: 'Collecting HFT shadow data…',
            hftFootL: 'Join vs offset · amend in place',
            hftFootR: 'Virtual execution only',
            action: 'Action',
            noPositionsShort: 'No positions',
            restApi: 'REST API',
            feeTier: 'Fee tier',
            crossDex: 'Cross-dex basis',
            strategySource: 'Strategy source',
            websocket: 'WebSocket',
            signerChain: 'Signer chain ID',
            running: 'Running',
            stable: 'Stable',
            version: 'Version',
            venue: 'Venue',
            leverageLimit: 'Leverage Limit',
            maxLeverageRisk: 'Max Leverage (Risk)',
            stopLossLabel: 'Stop Loss',
            takeProfitLabel: 'Take Profit',
            saveRisk: 'Save Risk Settings',
            secretKeep: 'leaving this blank keeps the current secret',
            hlAccount: 'Hyperliquid account address',
            hlSigner: 'Hyperliquid signer private key',
            lighterAccount: 'Lighter account index',
            lighterApi: 'Lighter API key index',
            lighterSecret: 'Lighter secret key',
            arcusKey: 'Arcus API key',
            arcusWallet: 'Arcus wallet address',
            arcusIndex: 'Arcus account index',
            arcusSign: 'Arcus signing key',
            asterWallet: 'Aster API wallet public address',
            asterSigner: 'Aster signer private key',
            rustLog: 'RUST_LOG',
            tokioThreads: 'TOKIO_WORKER_THREADS',
            unrealizedHeader: 'Unrealized P&L',
            applyChanges: 'Apply Changes',
            quantEngine: 'quant engine',
            pageTitle: 'Multi-Venue Quant Bot | Dashboard',
            brandName: 'Multi-Venue Quant Bot',
            noHft: 'No HFT profiles',
            toxic: 'TOXIC',
            lead: 'LEAD',
            csvExported: 'Trade history exported as CSV',
            activeMarketsChanged: 'Active markets changed',
            pushWord: 'PUSH',
            ordersWord: 'ORDERS',
            loopWord: 'LOOP',
            revWord: 'REV',
            state3s: 'state 3s',
            drawdownGate: 'drawdown gate',
            openOrdersHint: 'open orders',
            tradeHistoryHint: 'trade history',
            dailyCap: 'DAILY',
            openOrdersCount: 'open orders',
            waitingShadow: 'WAITING',
            networkLighter: 'Lighter Mainnet',
            networkRobinhood: 'Lighter · Robinhood Chain',
            networkArcus: 'Arcus Mainnet',
            networkArcusTest: 'Arcus Testnet',
            networkAster: 'Aster Mainnet',
            networkHl: 'Hyperliquid Mainnet',
            networkHlTest: 'Hyperliquid Testnet',
            lighterDesc: 'USDC · Crypto perpetuals',
            robinhoodDesc: 'USDG · Crypto & stock perpetuals',
            arcusDesc: 'USD · Equities, crypto & commodity perps',
            arcusTestDesc: 'Paper collateral · Integration testing',
            asterDesc: 'USDT · Aster Pro Futures V3',
            hlDesc: 'USDC · HIP-3 / entropy.io perps',
            hlTestDesc: 'Testnet USDC · HIP-3 coins when listed',
            ed25519: 'Ed25519 API key',
            apiWalletSigner: 'API Wallet signer',
            chainId: 'chain ID',
        },
        cn: {
            dashboard: '仪表盘', strategies: '策略', portfolio: '投资组合',
            history: '历史记录', settings: '设置', equity: '净值',
            unrealizedPnl: '未实现盈亏', dailyPnl: '当日盈亏', totalPnl: '总盈亏',
            equityCurve: '净值曲线', weeklyPnl: '周盈亏',
            positions: '持仓', orders: '订单', trades: '交易', log: '日志',
            orderBook: '订单簿', riskMonitor: '风控监控',
            maxDrawdown: '最大回撤', dailyLoss: '日内亏损',
            available: '可用余额', peakEquity: '峰值净值', openOrders: '挂单数',
            dailyPnlHistory: '每日盈亏历史',
            tradingControls: '交易控制', manageMarkets: '管理交易对和交易状态',
            activeMarkets: '激活市场', quickActions: '快速操作',
            saveMarketConfig: '保存市场配置', pauseTrading: '暂停交易',
            resumeTrading: '恢复交易', cancelAllOrders: '取消所有订单',
            performance: '业绩表现', winRate: '胜率', totalTrades: '总交易数',
            maxLeverage: '最大杠杆',
            investPerGrid: '每格投资 ($)', priceDeviation: '价格偏差 (%)',
            dcaSub: '定投策略', trendSub: 'EMA交叉 + RSI',
            buyInterval: '买入间隔 (小时)', amountPerBuy: '每次买入 ($)',
            dipThreshold: '下跌阈值 (%)', fastEma: '快速EMA周期',
            slowEma: '慢速EMA周期', rsiPeriod: 'RSI周期',
            tradeHistory: '交易历史', fullAuditTrail: '所有已执行交易的完整记录',
            exportCsv: '导出CSV', volume: '成交量', avgDuration: '平均持仓时间',
            systemStatus: '系统状态', botStatus: '机器人状态',
            apiConnection: 'API连接', riskLimits: '风险限制',
            theme: '主题', themeDesc: '切换亮色和暗色模式以获得舒适的浏览体验。',
            toggleTheme: '切换主题', market: '市场', side: '方向', size: '数量',
            entry: '开仓价', mark: '标记价', price: '价格', qty: '数量', filled: '已成交',
            status: '状态', time: '时间', asset: '资产', pnl: '盈亏',
            noPositions: '暂无持仓', noOrders: '暂无挂单',
            noTrades: '暂无交易', noHistory: '暂无交易历史',
            searchPlaceholder: '搜索...', searchByAsset: '按资产、方向搜索...',
            connecting: '连接中...', liveTrading: '实盘交易中', disconnected: '已断开',
            connectionLost: '连接断开，正在重连...',
            activateStrategy: '启用策略', stopLoss: '止损 (%)', takeProfit: '止盈 (%)',
            loopKicker: '两条控制回路 · WS 主循环 + 10s 刷新', theLoop: '控制回路',
            ordersKicker: '每一格都是一个挂单 · 实时来自 /ws',
            totalReturn: '累计收益率 · 自建仓起', lastPush: '最近推送',
            ordersPlaced: '下单次数', fillsCapped: '成交（最近 200 条）',
            legendPush: '每收到一帧 /ws state 推送', legendRisk: '回撤 / 日亏数值发生变化',
            legendOrder: '挂单数发生变化', legendFill: 'trade_history 新增记录',
            drawdownLimits: '回撤上限', refreshLoop: '实时 · 10s 刷新回路',
            legendDot: '跑一圈 = 一帧 /ws 推送（3s）', tapeIdle: '等待持仓数据', sinceFill: '距上次成交', railEmpty: '本次会话暂无事件',
            brandKicker: 'WS 进 \u00b7 订单出 \u00b7 两条控制回路', today: '今日',
            initial: '起始', headroom: '余量', headroomCap: '更紧的那条上限还剩多少',
            riskKicker: '还剩多少空间',
            equityFootL: '每个点都是一次 /api/pnl 快照', equityFootR: '区间在收益卡上切换',
            swarmFootL: '离市价最近的一档是反相卡', swarmFootR: '进度条 = 已成交 / 委托量',
            railKicker: '只记真实事件 \u00b7 最新在上',
            envKicker: '进程启动时读入 \u00b7 需重启生效', envTitle: '环境变量',
            networkKicker: '当前连接 \u00b7 切换需重启', networkTitle: '网络',
            networkCredentials: '各 venue 使用相互隔离的凭据。Aster V3 使用子账户 user 地址和 API Wallet signer。',
            saveNetwork: '重启后使用所选网络', networkFootL: '当前连接不会立即改变', networkFootR: '选择保存到 .env',
            envWarnTag: '安全',
            envWarn: '写操作接口受进程级 Dashboard 凭据保护；仍应只在可信网络访问。密钥为只写字段，后端不会把明文返回浏览器。',
            writeOnly: '（只写）', currentValue: '当前',
            secretHint: '只能填写交易所签发的 API 凭据，不要填写钱包 L1 私钥。密钥留空表示保持原值。',
            saveEnv: '保存网络凭据', envFootL: '单一文件 · 两组网络前缀隔离', envFootR: '重启机器人后生效', eventLog: '事件流', thisWeek: '本周 \u00b7 已实现',
            confirmCancel: '取消所有挂单？此操作不可撤销。', navMenu: '菜单',
            makerSub: '空仓双边 ALO · 成交后只平仓，超时 IOC 吃单',
            quoteNotional: '单笔名义 ($)', requoteCooldown: '换单冷却 (秒)',
            joinInside: '往中间收的 tick 数', flattenTimers: '平仓中间档 / IOC (秒)',
            flattenOnly: '成交后只平仓', applyMaker: '应用做市策略',
            noMarkets: '当前交易所还没有市场',
            mq_quote_mode: '报价模式',
            mq_per_quote_notional: '单笔名义 ($)',
            mq_min_quote_notional: '最小单笔名义 ($)',
            mq_total_quote_budget: '总报价预算 ($)',
            mq_soft_cap_notional: '库存软上限 ($)',
            mq_hard_cap_notional: '库存硬上限 ($)',
            mq_spread_bps: '价差 (bps，中间价模式)',
            mq_requote_threshold_bps: '换单阈值 (bps)',
            mq_requote_cooldown_secs: '换单冷却 (秒)',
            mq_join_inside_ticks: '往中间收的 tick 数',
            mq_ema_period: '趋势 EMA 周期',
            mq_trend_block_bps: '趋势封锁 (bps)',
            mq_max_skew_bps: '库存偏斜上限 (bps)',
            mq_vol_window: '波动窗口 (根, 0=关)',
            mq_vol_multiplier: '波动价差乘数',
            mq_min_book_spread_bps: '最小盘口价差 (bps)',
            mq_max_book_spread_bps: '最大盘口价差 (bps)',
            mq_wide_book_size_mult: '宽盘口尺寸乘数',
            mq_max_bbo_imbalance: '最大买卖不平衡 (倍, 0=关)',
            mq_jump_circuit_breaker_bps: '跳空熔断 (bps)',
            mq_circuit_breaker_cooldown_secs: '熔断冷却 (秒)',
            mq_feature_interval_secs: '特征采样间隔 (秒)',
            mq_flatten_mid_secs: '中间价减仓等待 (秒)',
            mq_flatten_ioc_secs: '转 IOC 平仓等待 (秒)',
            mq_cash_open_guard_before_minutes: '美股开盘前保护 (分)',
            mq_cash_open_guard_after_minutes: '美股开盘后保护 (分)',
            mq_trend_filter: '趋势过滤',
            mq_cash_open_guard: '美股开盘保护',
            mq_flatten_only: '成交后只平仓（超时 IOC）',
            mqJoinBest: '贴买一卖一',
            mqMidSpread: '中间价铺开',
            activeBadge: '● 运行中',
            inactiveBadge: '○ 未启用',
            pausedBadge: '⏸ 已暂停',
            applying: '正在应用...',
            applyFailed: '应用失败',
            applyMakerOk: '做市参数已应用',
            makerActivated: '做市策略已生效',
            gridActivated: '网格策略已启用',
            dcaActivated: '定投策略已启用',
            trendActivated: '趋势策略已启用',
            justNow: '刚刚',
            minutesAgo: ' 分钟前',
            hoursAgo: ' 小时前',
            daysAgo: ' 天前',
            noNotifs: '暂无通知',
            notifications: '通知',
            clearAll: '全部清除',
            rangeError: '必须介于',
            softCapError: '库存软上限不能大于硬上限',
            minQuoteError: '最小单笔名义不能大于单笔名义',
            marketsUpdated: '市场已更新',
            marketsFailed: '更新市场失败',
            tradingPausedMsg: '交易已暂停',
            tradingResumedMsg: '交易已恢复',
            actionFailed: '操作失败',
            allCancelled: '已取消全部挂单',
            cancelFailed: '取消挂单失败',
            riskSaved: '风控设置已保存',
            riskUpdated: '风控设置已更新',
            networkError: '网络错误',
            wsConnected: 'WebSocket 已连接',
            wsDisconnected: 'WebSocket 已断开，正在重连...',
            initLog: '仪表盘初始化中...',
            failedPnl: '盈亏数据加载失败',
            failedStrategy: '策略配置加载失败',
            noMatching: '没有匹配的交易',
            noClosed: '暂无已平仓记录',
            noDaily: '暂无每日数据',
            noData: '暂无数据',
            totalWord: '合计',
            closedTrades: '已平仓笔数',
            avgHold: '平均持仓',
            posSummary: '持仓盈亏汇总',
            gridStrategy: '网格策略',
            configuration: '参数配置',
            gridCount: '网格数量',
            live: '实盘',
            riskSlTp: '风控：止损 / 止盈',
            maxOpenOrders: '最大挂单数',
            dcaStrategy: '定投策略',
            trendFollowing: '趋势跟踪',
            makerQuote: '做市报价',
            loading: '加载中...',
            loadingMarkets: '正在加载市场…',
            ompAgent: 'OMP 助手',
            idle: '空闲',
            shadowKicker: '仅虚拟报价 · 风险平仓仍生效',
            asterShadow: 'Aster 影子做市',
            runtime: '运行时长',
            bboEvents: 'BBO 事件',
            eventLag: '事件延迟',
            depthLag: '深度延迟',
            strategyEval: '策略计算',
            queueAhead: '前方排队',
            quoteReqMin: '报价请求/分钟',
            amendSavings: '改单节省',
            virtualFills: '虚拟成交',
            virtualVolume: '虚拟成交额',
            volumeHour: '每小时成交额',
            hftKicker: '多配置 · 近盘口 · 不下真实单',
            hftLab: '高频影子实验室',
            profile: '配置',
            offset: '偏移',
            cooldown: '冷却',
            reqMin: '请求/分',
            amendSave: '改单节省',
            fills: '成交',
            volHour: '成交额/时',
            markout1s: '1秒标记损益',
            markout5s: '5秒标记损益',
            markout30s: '30秒标记损益',
            collectingHft: '正在采集高频影子数据…',
            hftFootL: '贴盘 vs 偏移 · 原地改单',
            hftFootR: '仅虚拟成交',
            action: '动作',
            noPositionsShort: '暂无持仓',
            restApi: 'REST 接口',
            feeTier: '费率档位',
            crossDex: '跨 dex 基差',
            strategySource: '策略来源',
            websocket: 'WebSocket',
            signerChain: '签名链 ID',
            running: '运行中',
            stable: '稳定',
            version: '版本',
            venue: '交易所',
            leverageLimit: '杠杆上限',
            maxLeverageRisk: '最大杠杆（风控）',
            stopLossLabel: '止损',
            takeProfitLabel: '止盈',
            saveRisk: '保存风控设置',
            secretKeep: '留空则保持现有密钥',
            hlAccount: 'Hyperliquid 账户地址',
            hlSigner: 'Hyperliquid 签名私钥',
            lighterAccount: 'Lighter 账户序号',
            lighterApi: 'Lighter API 密钥序号',
            lighterSecret: 'Lighter 密钥',
            arcusKey: 'Arcus API 密钥',
            arcusWallet: 'Arcus 钱包地址',
            arcusIndex: 'Arcus 账户序号',
            arcusSign: 'Arcus 签名密钥',
            asterWallet: 'Aster API 钱包公钥',
            asterSigner: 'Aster 签名私钥',
            rustLog: 'RUST_LOG',
            tokioThreads: 'TOKIO_WORKER_THREADS',
            unrealizedHeader: '未实现盈亏',
            applyChanges: '应用更改',
            quantEngine: '量化引擎',
            pageTitle: '多场所量化机器人 | 仪表盘',
            brandName: '多场所量化机器人',
            noHft: '暂无高频配置',
            toxic: '有毒',
            lead: '领先',
            csvExported: '交易历史已导出为 CSV',
            activeMarketsChanged: '已激活市场已变更',
            pushWord: '推送',
            ordersWord: '挂单',
            loopWord: '回路',
            revWord: '圈数',
            state3s: '状态 3秒',
            drawdownGate: '回撤闸门',
            openOrdersHint: '挂单',
            tradeHistoryHint: '成交记录',
            dailyCap: '日亏',
            openOrdersCount: '笔挂单',
            waitingShadow: '等待中',
            networkLighter: 'Lighter 主网',
            networkRobinhood: 'Lighter · Robinhood 链',
            networkArcus: 'Arcus 主网',
            networkArcusTest: 'Arcus 测试网',
            networkAster: 'Aster 主网',
            networkHl: 'Hyperliquid 主网',
            networkHlTest: 'Hyperliquid 测试网',
            lighterDesc: 'USDC · 加密永续',
            robinhoodDesc: 'USDG · 加密与股票永续',
            arcusDesc: 'USD · 股票、加密与商品永续',
            arcusTestDesc: '模拟保证金 · 联调测试',
            asterDesc: 'USDT · Aster Pro 期货 V3',
            hlDesc: 'USDC · HIP-3 / entropy.io 永续',
            hlTestDesc: '测试网 USDC · 已上线的 HIP-3 币对',
            ed25519: 'Ed25519 API 密钥',
            apiWalletSigner: 'API 钱包签名器',
            chainId: '链 ID',
        }
    };

    let currentLang = localStorage.getItem('lighter-lang') || 'cn';

    function t(key) { return (i18nStrings[currentLang] || i18nStrings.en)[key] || (i18nStrings.en)[key] || key; }

    function applyI18n() {
        // Update all elements with data-i18n attribute
        document.querySelectorAll('[data-i18n]').forEach(el => {
            const key = el.getAttribute('data-i18n');
            el.textContent = t(key);
        });
        // Update lang button
        const langLabel = $('lang-label');
        if (langLabel) langLabel.textContent = currentLang === 'en' ? 'EN' : '中';
        // Update placeholders
        const gs = $('global-search');
        if (gs) gs.placeholder = t('searchPlaceholder');
        const hs = $('h-search');
        if (hs) hs.placeholder = t('searchByAsset');
        document.querySelectorAll('[data-i18n-placeholder]').forEach(el => {
            el.placeholder = t(el.getAttribute('data-i18n-placeholder'));
        });
        // Update WS offline text
        const wsOff = $('ws-offline');
        if (wsOff) wsOff.textContent = t('connectionLost');
        if (document.title) document.title = t('pageTitle');
        renderMakerForm();
        if (lastMakerParams) fillMakerForm(lastMakerParams);
        if (typeof updatePauseButton === 'function') updatePauseButton();
        if (typeof updateConnectionStatus === 'function') updateConnectionStatus();
        if (lastStrategyName) updateStrategyBadges(lastStrategyName);
        renderNotifications();
        // 旁路面板有一批 JS 生成的文案（NO OPEN ORDERS / % FILLED …），
        // 它们不在 DOM 里带 data-i18n，只能靠这里推一次当前语言过去。
        try { if (window.__panels && window.__panels.lang) window.__panels.lang(currentLang); } catch (e) {}
    }

    function toggleLang() {
        currentLang = currentLang === 'en' ? 'cn' : 'en';
        localStorage.setItem('lighter-lang', currentLang);
        applyI18n();
    }

    if ($('btn-lang')) $('btn-lang').addEventListener('click', toggleLang);

    // ── Theme ──
    function getTheme() {
        const stored = localStorage.getItem('lighter-theme');
        if (stored) return stored;
        return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
    }

    function applyTheme(theme) {
        document.documentElement.setAttribute('data-theme', theme);
        localStorage.setItem('lighter-theme', theme);
        const icon = $('theme-icon');
        if (icon) {
            icon.setAttribute('data-lucide', theme === 'dark' ? 'sun' : 'moon');
            lucide.createIcons({ attrs: { id: 'theme-icon' } });
        }
        if (equityChart) updateChartTheme();
    }

    function toggleTheme() {
        const cur = document.documentElement.getAttribute('data-theme') || 'light';
        applyTheme(cur === 'dark' ? 'light' : 'dark');
    }

    applyTheme(getTheme());

    if ($('btn-theme')) $('btn-theme').addEventListener('click', toggleTheme);
    if ($('btn-theme-settings')) $('btn-theme-settings').addEventListener('click', toggleTheme);

    // 图表配色一律从 CSS 变量读。之前主题色在 CSS 和 JS 里各写了一份，
    // 换主题只改 CSS 会让图表留在旧配色上 —— 这个 helper 就是为了消掉那份副本。
    function cssVar(name, fallback) {
        const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
        return v || fallback;
    }
    function chartPalette() {
        return {
            grid: cssVar('--chart-grid', '#E3E1D8'),
            tick: cssVar('--chart-text', '#A09E93'),
            ink: cssVar('--primary', '#12120F'),
            onInk: cssVar('--on-primary', '#FFFFFF'),
            up: cssVar('--success', '#1B7F3B'),
            down: cssVar('--danger', '#C0392B'),
            fg: cssVar('--text-main', '#12120F'),
            bg: cssVar('--bg-card', '#FFFFFF'),
            dim: cssVar('--primary-dim', 'rgba(18,18,15,0.07)'),
        };
    }

    function updateChartTheme() {
        const p = chartPalette();
        const gridColor = p.grid;
        const tickColor = p.tick;
        // 线条/柱子的颜色也要跟着主题走，否则暗色下墨黑线画在黑底上直接消失
        if (equityChart) {
            const ds = equityChart.data.datasets[0];
            ds.borderColor = p.ink;
            ds.pointHoverBackgroundColor = p.ink;
        }
        if (revenueChart) {
            const bars = revenueChart.data.datasets[0];
            const vals = bars.data || [];
            bars.backgroundColor = vals.map(v => v >= 0 ? p.up : p.down);
        }
        [equityChart, revenueChart].forEach(c => {
            if (!c) return;
            if (c.options.scales.y) {
                c.options.scales.y.grid.color = gridColor;
                c.options.scales.y.ticks.color = tickColor;
            }
            if (c.options.scales.x) {
                c.options.scales.x.ticks.color = tickColor;
                if (c.options.scales.x.grid) c.options.scales.x.grid.color = gridColor;
            }
            c.update('none');
        });
    }

    // ── Clock ──
    function updateClock() {
        const now = new Date();
        const el = $('clock');
        if (el) el.textContent = now.toLocaleTimeString('en-GB', { hour12: false });
    }
    setInterval(updateClock, 1000);
    updateClock();

    // ── Notifications ──
    function addNotification(type, message) {
        notifications.unshift({ type, message, time: new Date() });
        if (notifications.length > 50) notifications.pop();
        const dot = $('notif-dot');
        if (dot) dot.style.display = '';
        renderNotifications();
    }

    function renderNotifications() {
        const list = $('notif-list');
        if (!list) return;
        if (notifications.length === 0) {
            list.innerHTML = '<div class="notif-empty">' + t('noNotifs') + '</div>';
            return;
        }
        list.innerHTML = notifications.slice(0, 20).map(n => {
            const iconClass = n.type === 'trade' ? 'trade' : n.type === 'warn' ? 'warn' : n.type === 'error' ? 'err' : 'trade';
            // 单色字形而不是彩色 emoji：.notif-icon 已经用 CSS 给了配色，
            // 彩色 emoji 会跟主题打架，而且各系统字形不一致。
            const iconChar = n.type === 'trade' ? '\u25b2' : n.type === 'warn' ? '!' : n.type === 'error' ? '\u00d7' : '\u00b7';
            const ago = timeAgo(n.time);
            return `<div class="notif-item"><div class="notif-icon ${iconClass}">${iconChar}</div><div class="notif-text"><div class="notif-msg">${escapeHtml(n.message)}</div><div class="notif-time">${ago}</div></div></div>`;
        }).join('');
    }

    function timeAgo(d) {
        const s = Math.floor((Date.now() - d.getTime()) / 1000);
        if (s < 60) return t('justNow');
        if (s < 3600) return Math.floor(s/60) + t('minutesAgo');
        if (s < 86400) return Math.floor(s/3600) + t('hoursAgo');
        return Math.floor(s/86400) + t('daysAgo');
    }

    if ($('btn-notif')) {
        $('btn-notif').addEventListener('click', e => {
            e.stopPropagation();
            const panel = $('notif-panel');
            panel.classList.toggle('show');
            if (panel.classList.contains('show')) {
                $('notif-dot').style.display = 'none';
            }
        });
    }
    if ($('notif-clear')) {
        $('notif-clear').addEventListener('click', () => {
            notifications = [];
            renderNotifications();
        });
    }
    document.addEventListener('click', () => {
        const p = $('notif-panel');
        if (p) p.classList.remove('show');
    });

    // ── Page Navigation ──
    document.querySelectorAll('.nav-item[data-page]').forEach(link => {
        link.addEventListener('click', function(e) {
            e.preventDefault();
            const page = this.getAttribute('data-page');
            if (page === activePage) return;
            document.querySelectorAll('.nav-item').forEach(l => l.classList.remove('active'));
            this.classList.add('active');
            document.querySelectorAll('.page').forEach(p => p.classList.remove('active'));
            const target = $('page-' + page);
            if (target) target.classList.add('active');
            $('current-page-title').innerText = this.innerText.trim();
            activePage = page;
            closeMobileNav();
            if (page === 'dashboard') setTimeout(initCharts, 100);
            if (page === 'history') { renderHistory(); renderPositionSummary(); }
        });
    });

    const navToggle = $('btn-nav-toggle');
    const navBackdrop = $('mobile-nav-backdrop');

    function closeMobileNav() {
        document.body.classList.remove('nav-open');
        if (navToggle) navToggle.setAttribute('aria-expanded', 'false');
    }

    if (navToggle) {
        navToggle.addEventListener('click', () => {
            const open = document.body.classList.toggle('nav-open');
            navToggle.setAttribute('aria-expanded', String(open));
            if (open) document.querySelector('#primary-navigation .nav-item')?.focus();
        });
    }
    if (navBackdrop) navBackdrop.addEventListener('click', closeMobileNav);
    document.addEventListener('keydown', event => {
        if (event.key === 'Escape') {
            const wasOpen = document.body.classList.contains('nav-open');
            closeMobileNav();
            if (wasOpen && navToggle) navToggle.focus();
        }
    });
    window.matchMedia('(min-width: 769px)').addEventListener('change', closeMobileNav);

    // ── Bottom Tabs ──
    const btmTabs = $('btm-tabs');
    if (btmTabs) {
        btmTabs.querySelectorAll('.tab-btn').forEach(btn => {
            btn.addEventListener('click', function() {
                btmTabs.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
                this.classList.add('active');
                const t = this.getAttribute('data-t');
                ['positions','orders','trades','log'].forEach(id => {
                    const panel = $('tp-' + id);
                    if (panel) panel.classList.toggle('active', id === t);
                });
            });
        });
    }

    // ── Search ──
    const globalSearch = $('global-search');
    if (globalSearch) {
        globalSearch.addEventListener('input', function() {
            const q = this.value.toLowerCase();
            if (activePage === 'history') {
                renderHistory(q);
            } else if (activePage === 'dashboard') {
                filterTable('pos-tbody', q);
                filterTable('ord-tbody', q);
                filterTable('trd-tbody', q);
            }
        });
    }

    function filterTable(tbodyId, query) {
        const tb = $(tbodyId);
        if (!tb) return;
        const rows = tb.querySelectorAll('tr');
        rows.forEach(r => {
            if (!query) { r.style.display = ''; return; }
            r.style.display = r.textContent.toLowerCase().includes(query) ? '' : 'none';
        });
    }

    function fmtDuration(seconds) {
        const total = Math.max(0, Math.floor(Number(seconds) || 0));
        if (!total) return '—';
        const days = Math.floor(total / 86400);
        const hours = Math.floor((total % 86400) / 3600);
        const minutes = Math.floor((total % 3600) / 60);
        if (days > 0) return `${days}d ${hours}h`;
        if (hours > 0) return `${hours}h ${minutes}m`;
        return `${minutes}m`;
    }

    function downsampleSeries(data, maxPoints) {
        if (!Array.isArray(data) || data.length <= maxPoints) return data || [];
        const sampled = [];
        const step = (data.length - 1) / (maxPoints - 1);
        for (let i = 0; i < maxPoints; i++) {
            sampled.push(data[Math.round(i * step)]);
        }
        return sampled;
    }

    function getVisibleEquityData() {
        if (!equityData.length || equityRange === 'all') return downsampleSeries(equityData, 900);
        const latestTs = equityData[equityData.length - 1].t;
        const windows = { '30d': 30 * 86400 * 1000, '7d': 7 * 86400 * 1000, '24h': 24 * 3600 * 1000 };
        const minTs = latestTs - (windows[equityRange] || 0);
        return downsampleSeries(equityData.filter(p => p.t >= minTs), 900);
    }

    function updateEquityRangeButtons() {
        document.querySelectorAll('#eq-range-group .range-btn').forEach(btn => {
            btn.classList.toggle('active', btn.getAttribute('data-range') === equityRange);
        });
    }

    function buildCloseTradeStats() {
        const ordered = [...allTrades].sort((a, b) => new Date(a.timestamp) - new Date(b.timestamp));
        const openTimes = {};
        const closeStats = [];

        ordered.forEach(t => {
            const symbol = t.symbol || t.market || 'Unknown';
            const action = tradeAction(t);
            const ts = new Date(t.timestamp).getTime();
            if (!Number.isFinite(ts)) return;

            if (isCloseAction(action)) {
                let durationSecs = Number(t.duration_secs || t.holding_duration_secs || 0);
                if (!durationSecs && openTimes[symbol] && openTimes[symbol].length) {
                    durationSecs = Math.max(0, Math.floor((ts - openTimes[symbol][0]) / 1000));
                }
                closeStats.push({
                    symbol,
                    pnl: Number(t.pnl || 0),
                    duration_secs: durationSecs,
                });
                if (isTerminalCloseAction(action) && openTimes[symbol]) {
                    openTimes[symbol] = [];
                }
                return;
            }

            if (hasTradePnl(t)) {
                closeStats.push({ symbol, pnl: Number(t.pnl), duration_secs: 0 });
                return;
            }

            if (/Open|Add/i.test(action)) {
                if (!openTimes[symbol]) openTimes[symbol] = [];
                if (action === 'Open' || openTimes[symbol].length === 0) {
                    openTimes[symbol] = [ts];
                }
            }
        });

        return closeStats;
    }

    document.querySelectorAll('#eq-range-group .range-btn').forEach(btn => {
        btn.addEventListener('click', function() {
            equityRange = this.getAttribute('data-range') || 'all';
            updateEquityRangeButtons();
            updateEquityChart();
        });
    });

    // ── History Filter ──
    let historyAssetFilter = 'all';
    document.querySelectorAll('.fpill[data-asset]').forEach(btn => {
        btn.addEventListener('click', function() {
            document.querySelectorAll('.fpill[data-asset]').forEach(b => b.classList.remove('active'));
            this.classList.add('active');
            historyAssetFilter = this.getAttribute('data-asset');
            renderHistory();
        });
    });

    const hSearch = $('h-search');
    if (hSearch) {
        hSearch.addEventListener('input', () => renderHistory());
    }

    // ── WebSocket ──
    // status-label must NOT use data-i18n="connecting": applyI18n() would
    // overwrite a live/paused pill back to "连接中" on every language apply.
    function updateConnectionStatus() {
        const dot = $('status-dot');
        const label = $('status-label');
        const setConn = $('set-conn');
        const wsOff = $('ws-offline');
        if (dot) dot.classList.toggle('live', !!wsConnected);
        if (label) {
            if (!wsConnected) {
                label.textContent = wsEverOpened ? t('disconnected') : t('connecting');
            } else if (typeof tradingPaused !== 'undefined' && tradingPaused) {
                label.textContent = t('pausedBadge');
            } else {
                label.textContent = t('liveTrading');
            }
        }
        if (setConn) {
            setConn.textContent = wsConnected ? t('wsConnected') : (wsEverOpened ? t('disconnected') : t('connecting'));
            setConn.style.color = wsConnected ? 'var(--success)' : 'var(--danger)';
        }
        if (wsOff) {
            wsOff.style.display = (!wsConnected && wsEverOpened) ? 'block' : 'none';
        }
    }
    function connect() {
        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        if (reconnTimer) { clearTimeout(reconnTimer); reconnTimer = null; }
        let opened = false;
        try {
            ws = new WebSocket(`${proto}//${location.host}/ws`);
        } catch (err) {
            wsConnected = false;
            wsEverOpened = true;
            updateConnectionStatus();
            addLog('w', t('wsDisconnected'));
            reconnTimer = setTimeout(connect, 5000);
            return;
        }
        const handshakeTimer = setTimeout(function () {
            if (!opened && ws && ws.readyState !== WebSocket.OPEN) {
                try { ws.close(); } catch (e) {}
            }
        }, 4000);
        ws.onopen = () => {
            opened = true;
            clearTimeout(handshakeTimer);
            wsConnected = true;
            wsEverOpened = true;
            updateConnectionStatus();
            addLog('i', t('wsConnected'));
            loadInitialData();
        };
        ws.onmessage = e => {
            try { handleMessage(JSON.parse(e.data)); }
            catch(err) { console.error('WS parse error:', err); }
        };
        ws.onerror = () => {
            wsConnected = false;
            wsEverOpened = true;
            updateConnectionStatus();
        };
        ws.onclose = () => {
            clearTimeout(handshakeTimer);
            wsConnected = false;
            wsEverOpened = true;
            updateConnectionStatus();
            addLog('w', t('wsDisconnected'));
            reconnTimer = setTimeout(connect, 5000);
        };
    }

    function handleMessage(msg) {
        switch (msg.type) {
            case 'status': updateMetrics(msg.data); break;
            case 'positions': updatePositions(msg.data); break;
            case 'recent_trades': updateTrades(msg.data); break;
            case 'open_orders': updateOrdersPanel(msg.data); break;
            case 'risk': updateRisk(msg.data); break;
        }
        // hero / THE LOOP / 挂单栅格的只读旁路，定义在 index.html 末尾的 <script> 里。
        // 放在 switch 之后，保证即使旁路抛错也不影响上面的主渲染路径。
        try { if (window.__panels) window.__panels.on(msg); } catch (e) { console.error('panels:', e); }
    }

    // ── Data Loading ──
    function loadInitialData() {
        fetch('/api/pnl').then(r => r.json()).then(data => {
            if (data.equity_history) {
                equityData = data.equity_history.map(p => ({ t: p.t * 1000, v: p.v }));
                updateEquityChart();
                const pnlMap = Object.assign({}, data.daily_pnl_map || {});
                const todayKey = new Date().toISOString().split('T')[0];
                if (data.daily_realized_pnl !== undefined) pnlMap[todayKey] = data.daily_realized_pnl;
                updateRevenueChart(pnlMap);
            }
            if (data.trades) {
                allTrades = data.trades;
                renderHistory();
                renderPositionSummary();
            }
            // History stats must use server lifetime counters — summing the
            // retained trade buffer under-reports after ring-buffer trims and
            // diverged from total_realized_pnl (e.g. -$0.74 vs +$17.27).
            applyServerHistoryStats(data);
            if (data.total_realized_pnl !== undefined) {
                const el = $('mc-total');
                if (el) { el.textContent = fmtPnl(data.total_realized_pnl); el.className = 'value ' + pnlClass(data.total_realized_pnl); }
                if ($('sp-pnl')) $('sp-pnl').textContent = fmtPnl(data.total_realized_pnl);
            }
        }).catch(e => addLog('e', t('failedPnl')));

        fetch('/api/strategy').then(r => r.json()).then(data => {
            if (data.params) {
                if ($('cfg-gc')) $('cfg-gc').value = data.params.grid_count || 6;
                if ($('cfg-inv')) $('cfg-inv').value = data.params.investment_per_grid || 8;
                if ($('cfg-dev')) $('cfg-dev').value = data.params.price_deviation || 0.012;
                fillMakerForm(data.params);
            }
            if (data.strategy && $('strat-name')) $('strat-name').textContent = data.strategy;
            updateStrategyBadges(data.strategy || 'grid_trading');
        }).catch(e => addLog('e', t('failedStrategy')));
    }

    // Refresh pnl-derived panels periodically: the full trade buffer and the
    // daily pnl map only arrive via /api/pnl, and WS recent_trades carries
    // just 20 rows. Deliberately does NOT refetch /api/strategy so in-flight
    // maker form edits are never overwritten.
    function refreshPnlStats() {
        fetch('/api/pnl').then(r => r.json()).then(data => {
            const pnlMap = Object.assign({}, data.daily_pnl_map || {});
            const todayKey = new Date().toISOString().split('T')[0];
            if (data.daily_realized_pnl !== undefined) pnlMap[todayKey] = data.daily_realized_pnl;
            updateRevenueChart(pnlMap);
            if (data.equity_history) {
                equityData = data.equity_history.map(p => ({ t: p.t * 1000, v: p.v }));
            }
            if (data.trades) {
                allTrades = data.trades;
                if (activePage === 'history') { renderHistory(); renderPositionSummary(); }
            }
            applyServerHistoryStats(data);
        }).catch(() => {});
    }
    setInterval(refreshPnlStats, 60000);

    // Update strategy card badges based on active strategy
    function updateStrategyBadges(active) {
        lastStrategyName = active;
        const gridBadge = document.querySelector('#strat-name')?.closest('.card')?.querySelector('.badge');
        const dcaBadge = $('dca-status-badge');
        const trendBadge = $('trend-status-badge');
        const gridOn = active === 'grid_trading' || active === 'grid';
        const makerOn = active === 'maker_quote' || active === 'maker';
        if (gridBadge) {
            gridBadge.className = 'badge ' + (gridOn ? 'badge-up' : 'badge-warn');
            gridBadge.textContent = gridOn ? t('activeBadge') : t('inactiveBadge');
        }
        const makerBadge = $('maker-status-badge');
        if (makerBadge) {
            makerBadge.className = 'badge ' + (makerOn ? 'badge-up' : 'badge-warn');
            makerBadge.textContent = makerOn ? t('activeBadge') : t('inactiveBadge');
        }
        if (dcaBadge) { dcaBadge.className = 'badge ' + (active === 'dca' ? 'badge-up' : 'badge-warn'); dcaBadge.textContent = active === 'dca' ? t('activeBadge') : t('inactiveBadge'); }
        if (trendBadge) { trendBadge.className = 'badge ' + (active === 'trend_following' || active === 'trend' ? 'badge-up' : 'badge-warn'); trendBadge.textContent = (active === 'trend_following' || active === 'trend') ? t('activeBadge') : t('inactiveBadge'); }
    }


    // ── Maker Quote: full parameter spec (mirrors trading.strategies.maker_quote) ──
    const MQ_PARAMS = [
        { key: 'quote_mode', type: 'select', options: ['join_best', 'mid_spread'], def: 'mid_spread' },
        { key: 'per_quote_notional', min: 1, max: 650, step: 1, def: 40 },
        { key: 'min_quote_notional', min: 1, max: 650, step: 1, def: 10 },
        { key: 'total_quote_budget', min: 10, max: 650, step: 10, def: 160 },
        { key: 'soft_cap_notional', min: 1, max: 650, step: 10, def: 80 },
        { key: 'hard_cap_notional', min: 1, max: 650, step: 10, def: 150 },
        { key: 'spread_bps', min: 1, max: 100, step: 0.5, def: 30 },
        { key: 'requote_threshold_bps', min: 0, max: 100, step: 0.1, def: 2 },
        { key: 'requote_cooldown_secs', min: 1, max: 300, step: 1, def: 20 },
        { key: 'join_inside_ticks', min: 0, max: 20, step: 1, def: 0 },
        { key: 'ema_period', min: 2, max: 500, step: 1, def: 20 },
        { key: 'trend_block_bps', min: 0, max: 100, step: 0.5, def: 8 },
        { key: 'max_skew_bps', min: 0, max: 100, step: 0.5, def: 12 },
        { key: 'vol_window', min: 0, max: 10000, step: 1, def: 0 },
        { key: 'vol_multiplier', min: 0, max: 5, step: 0.1, def: 0 },
        { key: 'min_book_spread_bps', min: 0, max: 500, step: 0.1, def: 1.5 },
        { key: 'max_book_spread_bps', min: 1, max: 500, step: 1, def: 80 },
        { key: 'wide_book_size_mult', min: 0, max: 10, step: 0.1, def: 1 },
        { key: 'max_bbo_imbalance', min: 0, max: 100, step: 0.5, def: 3 },
        { key: 'jump_circuit_breaker_bps', min: 1, max: 500, step: 1, def: 20 },
        { key: 'circuit_breaker_cooldown_secs', min: 1, max: 3600, step: 1, def: 60 },
        { key: 'feature_interval_secs', min: 1, max: 86400, step: 1, def: 60 },
        { key: 'flatten_mid_secs', min: 0, max: 120, step: 1, def: 6 },
        { key: 'flatten_ioc_secs', min: 1, max: 300, step: 1, def: 15 },
        { key: 'cash_open_guard_before_minutes', min: 0, max: 180, step: 1, def: 5 },
        { key: 'cash_open_guard_after_minutes', min: 0, max: 180, step: 1, def: 20 },
        { key: 'trend_filter', type: 'bool', def: true },
        { key: 'cash_open_guard', type: 'bool', def: true },
        { key: 'flatten_only', type: 'bool', def: true },
    ];
    function mqLabel(spec) { return t('mq_' + spec.key); }
    function mqOptionLabel(value) {
        if (value === 'join_best') return t('mqJoinBest');
        if (value === 'mid_spread') return t('mqMidSpread');
        return value;
    }

    function renderMakerForm() {
        const fields = $('mq-fields');
        const toggles = $('mq-toggles');
        if (!fields || !toggles) return;
        fields.innerHTML = '';
        toggles.innerHTML = '';
        MQ_PARAMS.forEach(spec => {
            const id = 'cfg-mq2-' + spec.key;
            if (spec.type === 'bool') {
                const label = document.createElement('label');
                label.className = 'market-toggle';
                label.innerHTML = '<input type="checkbox" id="' + id + '"' + (spec.def ? ' checked' : '') + '>' +
                    '<span class="mt-slider"></span><span class="mt-label">' + mqLabel(spec) + '</span>';
                toggles.appendChild(label);
                return;
            }
            const group = document.createElement('div');
            group.className = 'cfg-group';
            if (spec.type === 'select') {
                const opts = spec.options.map(o =>
                    '<option value="' + o + '"' + (o === spec.def ? ' selected' : '') + '>' + mqOptionLabel(o) + '</option>').join('');
                group.innerHTML = '<label class="cfg-label">' + mqLabel(spec) + '</label>' +
                    '<select class="cfg-input" id="' + id + '">' + opts + '</select>';
            } else {
                group.innerHTML = '<label class="cfg-label">' + mqLabel(spec) + '</label>' +
                    '<input type="number" class="cfg-input" id="' + id + '" value="' + spec.def +
                    '" min="' + spec.min + '" max="' + spec.max + '" step="' + spec.step + '">';
            }
            fields.appendChild(group);
        });
    }

    function fillMakerForm(params) {
        lastMakerParams = params;
        MQ_PARAMS.forEach(spec => {
            const el = $('cfg-mq2-' + spec.key);
            if (!el) return;
            const raw = params[spec.key];
            if (raw === undefined || raw === null || raw === '') return;
            if (spec.type === 'bool') {
                const on = String(raw).toLowerCase();
                el.checked = on === 'true' || on === '1';
            } else if (spec.type === 'select') {
                el.value = String(raw);
            } else {
                const num = parseFloat(raw);
                if (Number.isFinite(num)) el.value = num;
            }
        });
    }

    function collectMakerParams() {
        const params = {};
        const problems = [];
        MQ_PARAMS.forEach(spec => {
            const el = $('cfg-mq2-' + spec.key);
            if (!el) return;
            if (spec.type === 'bool') {
                params[spec.key] = el.checked;
            } else if (spec.type === 'select') {
                params[spec.key] = el.value;
            } else {
                const num = parseFloat(el.value);
                if (!Number.isFinite(num) || num < spec.min || num > spec.max) {
                    problems.push(mqLabel(spec) + ' ' + t('rangeError') + ' ' + spec.min + '–' + spec.max);
                } else {
                    params[spec.key] = num;
                }
            }
        });
        const softEl = $('cfg-mq2-soft_cap_notional'), hardEl = $('cfg-mq2-hard_cap_notional');
        if (softEl && hardEl && parseFloat(softEl.value) > parseFloat(hardEl.value)) {
            problems.push(t('softCapError'));
        }
        const minEl = $('cfg-mq2-min_quote_notional'), quoteEl = $('cfg-mq2-per_quote_notional');
        if (minEl && quoteEl && parseFloat(minEl.value) > parseFloat(quoteEl.value)) {
            problems.push(t('minQuoteError'));
        }
        return { params, problems };
    }

    renderMakerForm();

    const applyMakerBtn = $('btn-apply-maker');
    if (applyMakerBtn) {
        applyMakerBtn.addEventListener('click', function() {
            const { params, problems } = collectMakerParams();
            const msgEl = $('mq-msg');
            if (problems.length > 0) {
                msgEl.innerText = '\u2717 ' + problems[0];
                msgEl.style.color = 'var(--danger)';
                return;
            }
            const body = { strategy: 'maker_quote', params };
            this.disabled = true;
            fetch('/api/strategy', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
                .then(r => r.json())
                .then(() => {
                    msgEl.innerText = '✓ ' + t('applyMakerOk');
                    msgEl.style.color = 'var(--success)';
                    updateStrategyBadges('maker_quote');
                    addNotification('trade', t('makerActivated'));
                    setTimeout(() => msgEl.innerText = '', 3000);
                })
                .catch(() => { msgEl.innerText = '✗ ' + t('applyFailed'); msgEl.style.color = 'var(--danger)'; })
                .finally(() => { this.disabled = false; });
        });
    }

    // ── Strategy Apply (Grid) ──
    const applyBtn = $('btn-apply');
    if (applyBtn) {
        applyBtn.addEventListener('click', function() {
            const body = { strategy: 'grid_trading', params: {
                grid_count: parseFloat($('cfg-gc').value),
                investment_per_grid: parseFloat($('cfg-inv').value),
                price_deviation: parseFloat($('cfg-dev').value)
            }};
            this.disabled = true; this.innerText = t('applying');
            const msgEl = $('cfg-msg');
            fetch('/api/strategy', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
                .then(r => r.json())
                .then(d => {
                    msgEl.innerText = '✓ Grid strategy activated';
                    msgEl.style.color = 'var(--success)';
                    updateStrategyBadges('grid_trading');
                    addNotification('trade', t('gridActivated'));
                    setTimeout(() => msgEl.innerText = '', 3000);
                })
                .catch(() => { msgEl.innerText = '✗ ' + t('applyFailed'); msgEl.style.color = 'var(--danger)'; })
                .finally(() => { this.disabled = false; this.innerText = 'Apply Changes'; });
        });
    }

    // ── DCA Strategy Activate ──
    const dcaBtn = $('btn-activate-dca');
    if (dcaBtn) {
        dcaBtn.addEventListener('click', function() {
            const body = { strategy: 'dca', params: {
                interval: parseFloat($('cfg-dca-interval').value),
                amount: parseFloat($('cfg-dca-amount').value),
                dip_threshold: parseFloat($('cfg-dca-dip').value)
            }};
            this.disabled = true; this.innerText = 'Activating...';
            const msgEl = $('dca-msg');
            fetch('/api/strategy', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
                .then(r => r.json())
                .then(d => {
                    msgEl.innerText = '✓ DCA strategy activated';
                    msgEl.style.color = 'var(--success)';
                    updateStrategyBadges('dca');
                    addNotification('trade', t('dcaActivated'));
                    setTimeout(() => msgEl.innerText = '', 3000);
                })
                .catch(() => { msgEl.innerText = '✗ Failed'; msgEl.style.color = 'var(--danger)'; })
                .finally(() => { this.disabled = false; this.innerText = 'Activate DCA Strategy'; });
        });
    }

    // ── Trend Following Activate ──
    const trendBtn = $('btn-activate-trend');
    if (trendBtn) {
        trendBtn.addEventListener('click', function() {
            const body = { strategy: 'trend_following', params: {
                fast_ma: parseInt($('cfg-trend-fast').value),
                slow_ma: parseInt($('cfg-trend-slow').value),
                stop_loss: parseFloat($('cfg-trend-sl').value) / 100.0,
                take_profit: parseFloat($('cfg-trend-tp').value) / 100.0
            }};
            this.disabled = true; this.innerText = 'Activating...';
            const msgEl = $('trend-msg');
            fetch('/api/strategy', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) })
                .then(r => r.json())
                .then(d => {
                    msgEl.innerText = '✓ Trend strategy activated';
                    msgEl.style.color = 'var(--success)';
                    updateStrategyBadges('trend_following');
                    addNotification('trade', t('trendActivated'));
                    setTimeout(() => msgEl.innerText = '', 3000);
                })
                .catch(() => { msgEl.innerText = '✗ Failed'; msgEl.style.color = 'var(--danger)'; })
                .finally(() => { this.disabled = false; this.innerText = 'Activate Trend Strategy'; });
        });
    }

    // ── Metrics Update ──
    let lastEquity = 0, lastAvail = 0, lastPeak = 0;

    function updateMetrics(d) {
        if (!d) return;
        lastEquity = d.equity || 0;
        lastAvail = d.available_balance || 0;
        lastPeak = d.peak_equity || lastPeak;

        setVal('mc-equity', '$' + lastEquity.toFixed(2));
        setVal('pf-equity', '$' + lastEquity.toFixed(2));
        setVal('pf-avail', '$' + lastAvail.toFixed(2));
        setVal('s-avail', '$' + lastAvail.toFixed(2));
        setVal('s-peak', '$' + lastPeak.toFixed(2));

        const daily = d.daily_realized_pnl || 0;
        setPnl('mc-daily', daily);

        const upnl = d.unrealized_pnl || 0;
        setPnl('mc-upnl', upnl);

        const total = d.total_realized_pnl || 0;
        setPnl('mc-total', total);
        if ($('sp-pnl')) { $('sp-pnl').textContent = fmtPnl(total); $('sp-pnl').className = 'info-v ' + pnlClass(total); }

        // Keep History-page cards in sync with the same lifetime counters.
        applyServerHistoryStats(d);

        setVal('s-orders', d.open_orders || 0);
        setVal('s-orders-label', (d.open_orders || 0) + ' open orders');
        setVal('pf-ord-count', d.open_orders || 0);

        if (d.version) setVal('set-version', d.version);
        if (d.network) {
            var venueLabels = {
                'lighter-mainnet': 'Lighter Mainnet',
                'lighter-robinhood': 'Lighter · Robinhood Chain',
                'arcus-mainnet': 'Arcus Mainnet',
                'arcus-testnet': 'Arcus Testnet',
                'aster-mainnet': 'Aster Mainnet',
                'hyperliquid-mainnet': 'Hyperliquid Mainnet',
                'hyperliquid-testnet': 'Hyperliquid Testnet'
            };
            setVal('set-exchange', venueLabels[d.network] || d.network);
            setVal('hero-venue', venueLabels[d.network] || d.network);
        }
        if (d.strategy) setVal('strat-name', d.strategy);
        if (d.strategy) updateStrategyBadges(d.strategy);
        if (d.user_add_rate_bps != null && $('network-fee-tier')) {
            var addBps = Number(d.user_add_rate_bps);
            var crossBps = Number(d.user_cross_rate_bps || 0);
            var t4 = d.fee_tier_is_t4 === true;
            $('network-fee-tier').textContent = t4
                ? ('T4 · maker ' + addBps.toFixed(2) + ' bps')
                : ('T0 · maker ' + addBps.toFixed(2) + ' / taker ' + crossBps.toFixed(2) + ' bps');
        }
        if ($('network-cross-dex')) {
            if (d.last_cross_dex_net_bps != null) {
                $('network-cross-dex').textContent =
                    String(d.last_cross_dex_side || '') + ' · ' +
                    Number(d.last_cross_dex_net_bps).toFixed(2) + ' bps (not armed)';
            } else {
                $('network-cross-dex').textContent = currentLang === 'cn' ? '无（未武装）' : 'none (not armed)';
            }
        }
        if ($('network-strategy-source')) {
            const src = d.strategy_overlay
                ? (currentLang === 'cn' ? '覆盖文件' : 'overlay file')
                : 'yaml';
            const mode = d.quote_mode || '';
            const flat = String(d.flatten_only) === 'true' ? 'flatten_only' : '';
            $('network-strategy-source').textContent = [src, mode, flat].filter(Boolean).join(' · ');
        }

        // Equity chart update
        const now = Date.now();
        const lastPt = equityData[equityData.length - 1];
        if (!lastPt || now - lastPt.t > EQUITY_THROTTLE) {
            equityData.push({ t: now, v: lastEquity });
            if (equityData.length > MAX_EQUITY_PTS) equityData.shift();
            updateEquityChart();
        }

        // Chart info
        const visibleEquity = getVisibleEquityData();
        if ($('chart-info') && visibleEquity.length > 1) {
            const first = visibleEquity[0].v;
            const changePct = ((lastEquity - first) / first * 100);
            const labels = { all: 'ALL', '30d': '30D', '7d': '7D', '24h': '24H' };
            $('chart-info').textContent = `${labels[equityRange] || 'ALL'} ${fmtPct(changePct)} • ${visibleEquity.length}/${equityData.length} pts`;
            $('chart-info').style.color = changePct >= 0 ? 'var(--success)' : 'var(--danger)';
        }
    }

    function setVal(id, val) { const el = $(id); if (el) el.textContent = val; }
    function setPnl(id, val) {
        const el = $(id);
        if (!el) return;
        el.innerHTML = `<span class="pnl-arrow">${pnlArrow(val)}</span>${fmtPnl(val)}`;
        el.className = 'value ' + pnlClass(val);
        // Update parent stat-icon background tint
        const card = el.closest('.stat-card');
        if (card) {
            const icon = card.querySelector('.stat-icon');
            if (icon) {
                icon.style.color = val > 0.001 ? 'var(--success)' : val < -0.001 ? 'var(--danger)' : '';
                icon.style.background = val > 0.001 ? 'var(--success-bg)' : val < -0.001 ? 'var(--danger-bg)' : '';
            }
        }
    }

    // ── Positions ──
    function updatePositions(data) {
        const tb = $('pos-tbody');
        const pftb = $('pf-pos-tbody');
        if (!tb) return;
        const cnt = data ? data.length : 0;
        setVal('pf-pos-count', cnt);
        if (!data || cnt === 0) {
            const empty = '<tr><td colspan="6" class="empty-cell">No active positions</td></tr>';
            tb.innerHTML = empty;
            if (pftb) pftb.innerHTML = empty;
            return;
        }
        const html = data.map(p => {
            const pnl = p.unrealized_pnl || 0;
            return `<tr><td>${escapeHtml(p.symbol)}</td><td><span class="badge ${p.side==='Buy'?'badge-up':'badge-down'}">${escapeHtml(p.side)}</span></td><td>${escapeHtml(p.size)}</td><td>$${parseFloat(p.entry_price).toFixed(2)}</td><td>$${(p.mark_price||0).toFixed(2)}</td><td class="td-r ${pnlClass(pnl)}">${fmtPnl(pnl)}</td></tr>`;
        }).join('');
        tb.innerHTML = html;
        if (pftb) pftb.innerHTML = html;
    }

    // ── Orders ──
    function updateOrdersPanel(data) {
        if (Array.isArray(data)) {
            ordersData = data;
            const cnt = data.length;
            setVal('s-orders', cnt);
            setVal('s-orders-label', cnt + ' open orders');
            setVal('pf-ord-count', cnt);
            const tb = $('ord-tbody');
            const pftb = $('pf-ord-tbody');
            if (!tb) return;
            if (cnt === 0) {
                const empty = '<tr><td colspan="7" class="empty-cell">No open orders</td></tr>';
                tb.innerHTML = empty;
                if (pftb) pftb.innerHTML = empty;
                return;
            }
            const html = data.map(o => {
                const fill = o.filled_quantity || 0;
                const total = o.quantity || 1;
                const fillPct = (fill / total * 100).toFixed(0);
                return `<tr><td style="font-family:monospace;font-size:11px;">${escapeHtml(String(o.id).slice(-6))}</td><td>${escapeHtml(o.symbol || 'BTC')}</td><td><span class="badge ${o.side==='Buy'?'badge-up':'badge-down'}">${escapeHtml(o.side)}</span></td><td>$${parseFloat(o.price).toFixed(2)}</td><td>${escapeHtml(total)}</td><td>${escapeHtml(fill)} (${fillPct}%)</td><td><span class="badge badge-info">${escapeHtml(o.status || 'Open')}</span></td></tr>`;
            }).join('');
            tb.innerHTML = html;
            if (pftb) pftb.innerHTML = html;
        } else if (typeof data === 'number') {
            setVal('s-orders', data);
            setVal('s-orders-label', data + ' open orders');
        }
    }

    // ── Trades ──
    let prevTradeCount = 0;
    function updateTrades(data) {
        if (!data) return;
        // Detect new trades for notifications
        if (data.length > prevTradeCount && prevTradeCount > 0) {
            const newTrades = data.slice(0, data.length - prevTradeCount);
            newTrades.forEach(t => {
                const msg = `${t.side} ${t.symbol||t.market} @ $${parseFloat(t.price).toFixed(2)} × ${t.quantity}`;
                addNotification('trade', msg);
                addLog('t', 'Trade: ' + msg + (t.pnl ? ' PnL=' + fmtPnl(t.pnl) : ''));
            });
        }
        prevTradeCount = data.length;

        // Merge into allTrades (dedup by timestamp+price)
        data.forEach(t => {
            const exists = allTrades.some(a => a.timestamp === t.timestamp && a.price === t.price && a.side === t.side);
            if (!exists) allTrades.unshift(t);
        });

        const tb = $('trd-tbody');
        if (!tb) return;
        const rows = data.slice(0, 15);
        if (rows.length === 0) { tb.innerHTML = '<tr><td colspan="7" class="empty-cell">' + t('noTrades') + '</td></tr>'; return; }
        tb.innerHTML = rows.map(t => {
            const ts = new Date(t.timestamp).toLocaleTimeString();
            const pnl = t.pnl || 0;
            const action = tradeAction(t);
            const isClose = isCloseAction(action) || hasTradePnl(t);
            const actionBadge = isClose
                ? `<span class="badge ${pnl >= 0 ? 'badge-up' : 'badge-down'}">${escapeHtml(action)}</span>`
                : `<span class="badge badge-neutral">${escapeHtml(action)}</span>`;
            const pnlCell = isClose
                ? `<td class="td-r ${pnlClass(pnl)}">${fmtPnl(pnl)}</td>`
                : `<td class="td-r" style="color:var(--text-muted)">—</td>`;
            return `<tr><td>${ts}</td><td>${escapeHtml(t.symbol || t.market)}</td><td>${actionBadge}</td><td><span class="badge ${t.side==='Buy'?'badge-up':'badge-down'}">${escapeHtml(t.side)}</span></td><td>$${parseFloat(t.price).toFixed(2)}</td><td>${parseFloat(t.quantity).toFixed(6)}</td>${pnlCell}</tr>`;
        }).join('');
    }

    // ── Risk ──
    function updateRisk(data) {
        if (!data) return;
        const dd = data.drawdown_pct || 0;
        const dl = data.daily_loss_pct || 0;
        const ddLimit = data.max_drawdown_limit || 10;
        const dlLimit = data.daily_loss_limit || 5;

        setVal('r-dd', dd.toFixed(1) + '%');
        setVal('r-dl', dl.toFixed(1) + '%');

        const ddBar = $('r-dd-bar');
        const dlBar = $('r-dl-bar');
        if (ddBar) {
            ddBar.style.width = Math.min(dd / ddLimit * 100, 100) + '%';
            ddBar.className = 'risk-fill ' + (dd < ddLimit * 0.5 ? 'ok' : dd < ddLimit * 0.8 ? 'warn' : 'danger');
        }
        if (dlBar) {
            dlBar.style.width = Math.min(dl / dlLimit * 100, 100) + '%';
            dlBar.className = 'risk-fill ' + (dl < dlLimit * 0.5 ? 'ok' : dl < dlLimit * 0.8 ? 'warn' : 'danger');
        }
    }

    // ── History ──
    function renderHistory(searchOverride) {
        const tb = $('history-tbody');
        if (!tb) return;
        const search = (typeof searchOverride === 'string') ? searchOverride : ($('h-search') ? $('h-search').value.toLowerCase() : '');
        let filtered = allTrades;
        if (historyAssetFilter !== 'all') {
            filtered = filtered.filter(t => (t.symbol || t.market || '').toUpperCase().includes(historyAssetFilter));
        }
        if (search) {
            filtered = filtered.filter(t => {
                const txt = [t.symbol, t.market, t.side, t.price, t.quantity, t.action].join(' ').toLowerCase();
                return txt.includes(search);
            });
        }
        if (filtered.length === 0) {
            tb.innerHTML = '<tr><td colspan="7" class="empty-cell">' + t('noMatching') + '</td></tr>';
            return;
        }
        tb.innerHTML = filtered.slice(0, 100).map(t => {
            const ts = new Date(t.timestamp).toLocaleString();
            const pnl = t.pnl || 0;
            const action = tradeAction(t);
            const isClose = isCloseAction(action);
            const showPnl = isClose || hasTradePnl(t);
            const actionBadge = isClose
                ? `<span class="badge ${pnl >= 0 ? 'badge-up' : 'badge-down'}">${escapeHtml(action)}</span>`
                : `<span class="badge badge-neutral">${escapeHtml(action)}</span>`;
            const pnlCell = showPnl
                ? `<td class="td-r ${pnlClass(pnl)}">${fmtPnl(pnl)}</td>`
                : `<td class="td-r" style="color:var(--text-muted)">—</td>`;
            return `<tr><td>${ts}</td><td>${escapeHtml(t.symbol || t.market)}</td><td>${actionBadge}</td><td><span class="badge ${t.side==='Buy'?'badge-up':'badge-down'}">${escapeHtml(t.side)}</span></td><td>$${parseFloat(t.price).toFixed(2)}</td><td>${parseFloat(t.quantity).toFixed(6)}</td>${pnlCell}</tr>`;
        }).join('');
    }

    // Apply authoritative stats from /api/pnl. Volume and closed-trade count
    // are lifetime totals on the server; avg duration still comes from the
    // retained close events (older duration_secs are not stored separately).
    function applyServerHistoryStats(data) {
        if (!data) return;
        if (data.total_realized_pnl !== undefined) {
            setPnl('hc-pnl', data.total_realized_pnl);
        }
        const closeStatsForCount = buildCloseTradeStats();
        const closed = data.total_closed_trades;
        if (closed) {
            setVal('hc-closed-trades', closed);
        } else {
            // Hyperliquid never increments the server-side close counter;
            // fall back to pnl-bearing fills in the retained buffer.
            setVal('hc-closed-trades', closeStatsForCount.length);
        }
        const vol = data.total_volume;
        if (vol !== undefined && vol !== null) {
            setVal('hc-volume', '$' + Number(vol).toFixed(0));
        }
        // Visible-buffer length for "recent fills"; total_trades if present.
        if (data.total_trades !== undefined) {
            setVal('sp-trades', data.total_trades);
        } else if (data.trade_history_len !== undefined) {
            setVal('sp-trades', data.trade_history_len);
        } else if (allTrades.length) {
            setVal('sp-trades', allTrades.length);
        }
        const avgDurationSecs = closeStatsForCount.length
            ? closeStatsForCount.reduce((sum, t) => sum + (t.duration_secs || 0), 0) / closeStatsForCount.length
            : 0;
        setVal('hc-duration', fmtDuration(avgDurationSecs));
    }

    function computeHistoryStats() {
        // Fallback when only the local buffer is available (e.g. WS-only path).
        if (!allTrades.length) return;
        let totalPnl = 0, closeTrades = 0, vol = 0;
        allTrades.forEach(t => {
            const isClose = isCloseAction(tradeAction(t));
            vol += Math.abs(parseFloat(t.price) * parseFloat(t.quantity));
            if (isClose) {
                totalPnl += t.pnl || 0;
                closeTrades++;
            }
        });
        const closeStats = buildCloseTradeStats();
        const avgDurationSecs = closeStats.length
            ? closeStats.reduce((sum, t) => sum + (t.duration_secs || 0), 0) / closeStats.length
            : 0;
        setPnl('hc-pnl', totalPnl);
        setVal('hc-closed-trades', closeTrades);
        setVal('hc-volume', '$' + vol.toFixed(0));
        setVal('hc-duration', fmtDuration(avgDurationSecs));
        setVal('sp-trades', allTrades.length);
    }

    function renderPositionSummary() {
        const tb = $('pos-summary-tbody');
        if (!tb) return;
        const closeTrades = buildCloseTradeStats();
        if (closeTrades.length === 0) {
            tb.innerHTML = '<tr><td colspan="4" class="empty-cell">' + t('noClosed') + '</td></tr>';
            return;
        }
        const groups = {};
        closeTrades.forEach(t => {
            const asset = t.symbol || 'Unknown';
            if (!groups[asset]) groups[asset] = { totalPnl: 0, count: 0, durationSum: 0 };
            const pnl = t.pnl || 0;
            groups[asset].count++;
            groups[asset].totalPnl += pnl;
            groups[asset].durationSum += Number(t.duration_secs || 0);
        });
        let totalPnl = 0, totalCount = 0, totalDuration = 0;
        const rows = Object.entries(groups).map(([asset, g]) => {
            totalPnl += g.totalPnl;
            totalCount += g.count;
            totalDuration += g.durationSum;
            return `<tr><td><b>${escapeHtml(asset)}</b></td><td>${g.count}</td><td>${fmtDuration(g.durationSum / Math.max(g.count, 1))}</td><td class="td-r ${pnlClass(g.totalPnl)}">${fmtPnl(g.totalPnl)}</td></tr>`;
        });
        rows.push(`<tr style="border-top:2px solid var(--border);font-weight:600;"><td>${t('totalWord')}</td><td>${totalCount}</td><td>${fmtDuration(totalDuration / Math.max(totalCount, 1))}</td><td class="td-r ${pnlClass(totalPnl)}">${fmtPnl(totalPnl)}</td></tr>`);
        tb.innerHTML = rows.join('');
    }

    // ── Export CSV ──
    if ($('btn-export')) {
        $('btn-export').addEventListener('click', () => {
            let csv = 'Time,Asset,Side,Price,Quantity,PNL\n';
            allTrades.forEach(t => {
                csv += `"${new Date(t.timestamp).toISOString()}","${t.symbol||t.market}","${t.side}",${t.price},${t.quantity},${t.pnl||0}\n`;
            });
            const blob = new Blob([csv], { type: 'text/csv' });
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url; a.download = 'quant_trades_' + new Date().toISOString().slice(0,10) + '.csv';
            document.body.appendChild(a); a.click(); document.body.removeChild(a);
            URL.revokeObjectURL(url);
            addNotification('trade', t('csvExported'));
        });
    }

    // ── Daily PnL History Bars ──
    function renderPnlHistory(pnlMap) {
        const el = $('pnl-history');
        if (!el) return;
        const entries = Object.entries(pnlMap).sort((a, b) => b[0].localeCompare(a[0])).slice(0, 14);
        if (entries.length === 0) { el.innerHTML = '<div class="notif-empty">' + t('noDaily') + '</div>'; return; }
        const maxAbs = Math.max(...entries.map(e => Math.abs(e[1])), 0.01);
        el.innerHTML = entries.map(([date, val]) => {
            const pct = (Math.abs(val) / maxAbs * 48).toFixed(1);
            const isPos = val >= 0;
            const shortDate = date.slice(5);
            return `<div class="pnl-bar-row"><span class="pnl-bar-date">${escapeHtml(shortDate)}</span><div class="pnl-bar-track"><div class="pnl-bar-center"></div><div class="pnl-bar-fill ${isPos?'pos':'neg'}" style="width:${pct}%;${isPos?'':'right:auto;left:calc(50% - '+pct+'%);'}"></div></div><span class="pnl-bar-val ${pnlClass(val)}">${fmtPnl(val)}</span></div>`;
        }).join('');
    }

    // ── Log ──
    let logLines = [];
    function addLog(level, msg) {
        const ts = new Date().toLocaleTimeString('en-GB', { hour12: false });
        logLines.push({ ts, level, msg });
        if (logLines.length > 200) logLines.shift();
        const box = $('log-box');
        if (!box) return;
        box.innerHTML = logLines.slice(-60).map(l => {
            const safeLevel = ['i', 'w', 'e', 't'].includes(l.level) ? l.level : 'i';
            return `<div class="log-line"><span class="log-ts">[${l.ts}]</span> <span class="log-${safeLevel}">${escapeHtml(l.msg)}</span></div>`;
        }).join('');
        box.scrollTop = box.scrollHeight;
    }

    // ── Charts ──
    function initCharts() {
        const pal = chartPalette();
        const gridColor = pal.grid;
        const tickColor = pal.tick;
        const primaryColor = pal.ink;
        const visibleEquity = getVisibleEquityData();
        updateEquityRangeButtons();

        const ctxEq = $('equityChart');
        if (!ctxEq) return;
        if (equityChart) equityChart.destroy();

        equityChart = new Chart(ctxEq.getContext('2d'), {
            type: 'line',
            data: {
                labels: visibleEquity.map(d => {
                    const dt = new Date(d.t);
                    return dt.toLocaleDateString(undefined, {month:'short', day:'numeric'}) + ' ' +
                        dt.toLocaleTimeString(undefined, {hour:'2-digit', minute:'2-digit'});
                }),
                datasets: [{
                    label: 'Equity',
                    data: visibleEquity.map(d => d.v),
                    borderColor: primaryColor,
                    borderWidth: 2.5,
                    fill: true,
                    backgroundColor: (ctx) => {
                        const chart = ctx.chart;
                        const { ctx: c, chartArea } = chart;
                        if (!chartArea) return null;
                        const g = c.createLinearGradient(0, chartArea.top, 0, chartArea.bottom);
                        // --primary-dim 本身就是带 alpha 的墨色，两个主题下都已经调好
                        g.addColorStop(0, chartPalette().dim);
                        g.addColorStop(1, 'transparent');
                        return g;
                    },
                    tension: 0.4,
                    pointRadius: 0,
                    pointHoverRadius: 4,
                    pointHoverBackgroundColor: primaryColor,
                }]
            },
            options: {
                responsive: true, maintainAspectRatio: false,
                interaction: { mode: 'index', intersect: false },
                plugins: {
                    legend: { display: false },
                    tooltip: {
                        backgroundColor: pal.ink, titleColor: pal.onInk, bodyColor: pal.onInk,
                        padding: 9, cornerRadius: 6,
                        displayColors: false,
                        callbacks: { label: ctx => '$' + ctx.parsed.y.toFixed(2) }
                    }
                },
                scales: {
                    x: { display: true, grid: { display: false }, ticks: { color: tickColor, font: { size: 10 }, maxTicksLimit: 6, maxRotation: 0 } },
                    y: { grid: { color: gridColor, drawBorder: false }, ticks: { color: tickColor, font: { weight: '500' } } }
                }
            }
        });

        const ctxRev = $('revenueChart');
        if (!ctxRev) return;
        if (revenueChart) revenueChart.destroy();
        revenueChart = new Chart(ctxRev.getContext('2d'), {
            type: 'bar',
            data: {
                labels: ['Mon','Tue','Wed','Thu','Fri','Sat','Sun'],
                datasets: [{ label: 'P&L', data: [0,0,0,0,0,0,0], backgroundColor: primaryColor, borderRadius: 3 }]
            },
            options: {
                responsive: true, maintainAspectRatio: false,
                plugins: {
                    legend: { display: false },
                    tooltip: {
                        backgroundColor: pal.ink, titleColor: pal.onInk, bodyColor: pal.onInk,
                        padding: 9, cornerRadius: 6,
                        callbacks: { label: ctx => fmtPnl(ctx.parsed.y) }
                    }
                },
                scales: {
                    x: { grid: { display: false }, ticks: { color: tickColor } },
                    y: { grid: { color: gridColor, drawBorder: false }, ticks: { display: false } }
                }
            }
        });
    }

    function updateEquityChart() {
        if (!equityChart) return;
        const visibleEquity = getVisibleEquityData();
        equityChart.data.labels = visibleEquity.map(d => {
            const dt = new Date(d.t);
            return dt.toLocaleDateString(undefined, {month:'short', day:'numeric'}) + ' ' +
                dt.toLocaleTimeString(undefined, {hour:'2-digit', minute:'2-digit'});
        });
        equityChart.data.datasets[0].data = visibleEquity.map(d => d.v);
        equityChart.update('none');
        if ($('chart-info') && visibleEquity.length > 1) {
            const first = visibleEquity[0].v;
            const last = visibleEquity[visibleEquity.length - 1].v;
            const labels = { all: 'ALL', '30d': '30D', '7d': '7D', '24h': '24H' };
            const changePct = ((last - first) / first) * 100;
            $('chart-info').textContent = `${labels[equityRange] || 'ALL'} ${fmtPct(changePct)} • ${visibleEquity.length}/${equityData.length} pts`;
            $('chart-info').style.color = changePct >= 0 ? 'var(--success)' : 'var(--danger)';
        }
    }

    function updateRevenueChart(pnlMap) {
        // Bars must not depend on Chart.js: on first load the WS connects
        // before initCharts (500ms), and the early return left the daily
        // pnl panel stuck on the loading placeholder forever.
        renderPnlHistory(pnlMap);
        if (!revenueChart) return;
        const values = new Array(7).fill(0);
        const today = new Date();
        const dayOfWeek = (today.getDay() + 6) % 7;
        for (let i = 0; i < 7; i++) {
            const d = new Date();
            d.setDate(today.getDate() - dayOfWeek + i);
            const key = d.toISOString().split('T')[0];
            values[i] = pnlMap[key] || 0;
        }
        revenueChart.data.datasets[0].data = values;
        // Chart.js 不解析 CSS 变量，原来负值传 'var(--danger)' 会被当成无效色画成透明
        const revPal = chartPalette();
        revenueChart.data.datasets[0].backgroundColor = values.map(v => v >= 0 ? revPal.up : revPal.down);
        revenueChart.update();
        renderPnlHistory(pnlMap);
    }

    // ── Trading Controls ──
    let tradingPaused = false;
    let activeMarketsSet = new Set();
    const marketNames = {};

    function loadTradingControls() {
        fetch('/api/trading/markets').then(r => r.json()).then(data => {
            const host = $('tc-markets');
            const available = (data && data.available_markets) || [];
            activeMarketsSet = new Set(data.active_markets || []);
            available.forEach(m => { marketNames[m.id] = m.symbol; });
            if (host) {
                if (!available.length) {
                    host.innerHTML = '<div class="empty-cell" id="tc-markets-empty">' + t('noMarkets') + '</div>';
                } else {
                    host.innerHTML = available.map(m => {
                        const checked = activeMarketsSet.has(m.id) ? ' checked' : '';
                        return '<label class="market-toggle"><input type="checkbox" id="tc-m-' + m.id + '" data-market="' + m.id + '"' + checked + '><span class="mt-slider"></span><span class="mt-label">' + escapeHtml(m.symbol) + ' <span class="mt-id">(' + m.id + ')</span></span></label>';
                    }).join('');
                }
            }
            updateMarketsDisplay();
            if (data.trading_paused !== undefined) {
                tradingPaused = data.trading_paused;
                updatePauseButton();
            }
        }).catch(() => {});
    }

    function updateMarketsDisplay() {
        const names = [...activeMarketsSet].map(m => marketNames[m] || 'M' + m).join(', ');
        setVal('sp-markets', names || 'None');
    }

    function updatePauseButton() {
        const btn = $('btn-pause-trading');
        const txt = $('btn-pause-text');
        const badge = $('tc-status-badge');
        if (!btn) { updateConnectionStatus(); return; }
        if (tradingPaused) {
            btn.classList.add('paused');
            txt.textContent = t('resumeTrading');
            if (badge) { badge.textContent = t('pausedBadge'); badge.className = 'badge badge-warn'; }
        } else {
            btn.classList.remove('paused');
            txt.textContent = t('pauseTrading');
            if (badge) { badge.textContent = t('activeBadge'); badge.className = 'badge badge-up'; }
        }
        updateConnectionStatus();
    }

    // Save markets
    if ($('btn-save-markets')) {
        $('btn-save-markets').addEventListener('click', () => {
            const markets = [];
            document.querySelectorAll('#tc-markets input[data-market]:checked').forEach(cb => {
                markets.push(parseInt(cb.getAttribute('data-market')));
            });
            const msgEl = $('tc-market-msg');
            fetch('/api/trading/markets', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ markets })
            }).then(r => r.json()).then(data => {
                activeMarketsSet = new Set(markets);
                updateMarketsDisplay();
                msgEl.textContent = '✓ ' + data.message;
                msgEl.style.color = 'var(--success)';
                addNotification('trade', t('marketsUpdated') + ': ' + markets.map(m => marketNames[m] || m).join(', '));
                addLog('i', 'Active markets changed: ' + JSON.stringify(markets));
                setTimeout(() => msgEl.textContent = '', 3000);
            }).catch(() => {
                msgEl.textContent = '✗ ' + t('marketsFailed');
                msgEl.style.color = 'var(--danger)';
            });
        });
    }

    // Pause/Resume
    if ($('btn-pause-trading')) {
        $('btn-pause-trading').addEventListener('click', () => {
            const endpoint = tradingPaused ? '/api/trading/resume' : '/api/trading/pause';
            const msgEl = $('tc-action-msg');
            fetch(endpoint, { method: 'POST' }).then(r => r.json()).then(data => {
                tradingPaused = !tradingPaused;
                updatePauseButton();
                msgEl.textContent = '✓ ' + data.message;
                msgEl.style.color = 'var(--success)';
                addNotification(tradingPaused ? 'warn' : 'trade', tradingPaused ? t('tradingPausedMsg') : t('tradingResumedMsg'));
                addLog(tradingPaused ? 'w' : 'i', tradingPaused ? 'Trading PAUSED' : 'Trading RESUMED');
                setTimeout(() => msgEl.textContent = '', 3000);
            }).catch(() => {
                msgEl.textContent = '✗ ' + t('actionFailed');
                msgEl.style.color = 'var(--danger)';
            });
        });
    }

    // Cancel All
    if ($('btn-cancel-all')) {
        $('btn-cancel-all').addEventListener('click', () => {
            if (!confirm(t('confirmCancel'))) return;
            const msgEl = $('tc-action-msg');
            fetch('/api/trading/cancel-all', { method: 'POST' }).then(r => r.json()).then(data => {
                msgEl.textContent = '✓ ' + data.message;
                msgEl.style.color = 'var(--success)';
                addNotification('warn', t('allCancelled'));
                addLog('w', 'Cancel all orders requested');
                setTimeout(() => msgEl.textContent = '', 3000);
            }).catch(() => {
                msgEl.textContent = '✗ ' + t('cancelFailed');
                msgEl.style.color = 'var(--danger)';
            });
        });
    }

    // Update paused state from WS status messages
    const origUpdateMetrics = updateMetrics;
    updateMetrics = function(d) {
        origUpdateMetrics(d);
        if (d && d.trading_paused !== undefined && d.trading_paused !== tradingPaused) {
            tradingPaused = d.trading_paused;
            updatePauseButton();
        }
        if (d && d.active_markets) {
            activeMarketsSet = new Set(d.active_markets);
            updateMarketsDisplay();
        }
    };

    // ── Risk Config ──
    function loadRiskConfig() {
        fetch('/api/risk/config').then(r => r.json()).then(data => {
            if (data.leverage_limit !== undefined) setRcInput('rc-leverage-limit', data.leverage_limit);
            if (data.max_leverage !== undefined) setRcInput('rc-max-leverage', data.max_leverage);
            if (data.position_stop_loss_pct !== undefined) setRcInput('rc-stop-loss', data.position_stop_loss_pct);
            if (data.position_take_profit_pct !== undefined) setRcInput('rc-take-profit', data.position_take_profit_pct);
            if (data.max_drawdown_pct !== undefined) setRcInput('rc-max-drawdown', data.max_drawdown_pct);
            if (data.daily_loss_limit_pct !== undefined) setRcInput('rc-daily-loss', data.daily_loss_limit_pct);
        }).catch(() => {});
    }

    function setRcInput(id, val) {
        const el = $(id);
        if (el) el.value = val;
    }

    // Save risk config
    if ($('btn-save-risk')) {
        $('btn-save-risk').addEventListener('click', () => {
            const body = {
                leverage_limit: parseFloat($('rc-leverage-limit').value) || 3,
                max_leverage: parseFloat($('rc-max-leverage').value) || 5,
                position_stop_loss_pct: parseFloat($('rc-stop-loss').value) || 3,
                position_take_profit_pct: parseFloat($('rc-take-profit').value) || 5,
                max_drawdown_pct: parseFloat($('rc-max-drawdown').value) || 10,
                daily_loss_limit_pct: parseFloat($('rc-daily-loss').value) || 5,
            };
            const msgEl = $('rc-save-msg');
            fetch('/api/risk/config', {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify(body)
            }).then(r => r.json()).then(data => {
                if (data.status === 'ok') {
                    msgEl.textContent = '✓ ' + t('riskSaved');
                    msgEl.style.color = 'var(--success)';
                    addNotification('trade', t('riskUpdated'));
                    addLog('i', 'Risk config updated: leverage=' + body.leverage_limit + 'x, SL=' + body.position_stop_loss_pct + '%, TP=' + body.position_take_profit_pct + '%');
                } else {
                    msgEl.textContent = '✗ ' + (data.message || 'Failed');
                    msgEl.style.color = 'var(--danger)';
                }
                setTimeout(() => msgEl.textContent = '', 4000);
            }).catch(e => {
                msgEl.textContent = '✗ ' + t('networkError');
                msgEl.style.color = 'var(--danger)';
            });
        });
    }

    // ── Init ──
    addLog('i', t('initLog'));
    applyI18n();
    connect();
    setTimeout(initCharts, 500);
    setTimeout(loadTradingControls, 1000);
    setTimeout(loadRiskConfig, 1200);

})();
