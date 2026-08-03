/// 计算 EMA 序列的最后两个值 (prev, current)
pub(super) fn ema_last_two(prices: &[f64], period: usize) -> Option<(f64, f64)> {
    if prices.len() < period + 1 {
        return None;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let seed: f64 = prices[..period].iter().sum::<f64>() / period as f64;
    let mut ema = seed;
    let mut prev = seed;
    for &p in &prices[period..] {
        prev = ema;
        ema = alpha * p + (1.0 - alpha) * ema;
    }
    Some((prev, ema))
}

/// 计算完整 EMA 序列（长度 = prices.len()）。
/// 前 period 根为 SMA 种子，之后逐根 EMA 平滑。用于斜率确认（slow EMA 的运动方向）。
pub(super) fn ema_series(prices: &[f64], period: usize) -> Option<Vec<f64>> {
    if prices.len() < period {
        return None;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(prices.len());
    let seed: f64 = prices[..period].iter().sum::<f64>() / period as f64;
    for (i, &p) in prices.iter().enumerate() {
        if i < period {
            out.push(seed);
        } else {
            let prev = *out.last().unwrap();
            out.push(alpha * p + (1.0 - alpha) * prev);
        }
    }
    Some(out)
}

/// ADX（Average Directional Index），Wilder 平滑，衡量趋势强度。
/// 返回 None 当数据不足以 warm up（需要 len > 2*period）。
/// ADX 低于阈值 → 无趋势/震荡市，可用于过滤进场信号。
pub(super) fn adx(highs: &[f64], lows: &[f64], closes: &[f64], period: usize) -> Option<f64> {
    if period == 0 || highs.len() != lows.len() || highs.len() != closes.len() {
        return None;
    }
    let n = highs.len();
    // 需要至少 period+1 根算第一条 TR/DM，再 period 根 Wilder 平滑才够第一条 DX。
    if n <= period + 1 {
        return None;
    }

    // 逐根 TR / +DM / -DM（从 i=1 起，依赖 i-1）
    let count = n - 1;
    let mut tr = Vec::with_capacity(count);
    let mut pdm = Vec::with_capacity(count);
    let mut mdm = Vec::with_capacity(count);
    for i in 1..n {
        let hl = highs[i] - lows[i];
        let hc = (highs[i] - closes[i - 1]).abs();
        let lc = (lows[i] - closes[i - 1]).abs();
        tr.push(hl.max(hc.max(lc)));
        let up = highs[i] - highs[i - 1];
        let down = lows[i - 1] - lows[i];
        pdm.push(if up > down && up > 0.0 { up } else { 0.0 });
        mdm.push(if down > up && down > 0.0 { down } else { 0.0 });
    }

    let p = period as f64;
    let mut atr = tr.clone();
    let mut apdm = pdm.clone();
    let mut amdm = mdm.clone();
    atr[period..].fill(0.0);
    apdm[period..].fill(0.0);
    amdm[period..].fill(0.0);
    for i in period..count {
        atr[i] = (atr[i - 1] * (p - 1.0) + tr[i]) / p;
        apdm[i] = (apdm[i - 1] * (p - 1.0) + pdm[i]) / p;
        amdm[i] = (amdm[i - 1] * (p - 1.0) + mdm[i]) / p;
    }

    // DX 序列
    let mut dx = vec![0.0; count];
    for i in 0..count {
        if atr[i] <= 0.0 {
            dx[i] = 0.0;
            continue;
        }
        let pdi = apdm[i] / atr[i] * 100.0;
        let mdi = amdm[i] / atr[i] * 100.0;
        let sum = pdi + mdi;
        dx[i] = if sum > 0.0 {
            (pdi - mdi).abs() / sum * 100.0
        } else {
            0.0
        };
    }

    // ADX = Wilder 平滑 DX，前 period 平均作种子
    let seed: f64 = dx[..period].iter().sum::<f64>() / p;
    let mut adx_val = seed;
    let mut prev = seed;
    for &d in &dx[period..] {
        prev = adx_val;
        adx_val = (adx_val * (p - 1.0) + d) / p;
    }
    Some((prev + adx_val) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adx_rejects_insufficient_data() {
        let h = [1.0, 2.0, 3.0];
        let l = [0.9, 1.9, 2.9];
        let c = [1.0, 2.0, 3.0];
        assert!(adx(&h, &l, &c, 14).is_none());
    }

    #[test]
    fn adx_strong_trend_is_high() {
        // 持续单边上涨 → ADX 应明显高于震荡序列
        let mut h = Vec::new();
        let mut l = Vec::new();
        let mut c = Vec::new();
        let mut price = 100.0;
        for _ in 0..60 {
            price += 1.0;
            h.push(price + 0.2);
            l.push(price - 0.2);
            c.push(price);
        }
        let trend = adx(&h, &l, &c, 14).expect("should compute");
        assert!(trend > 25.0, "trend ADX too low: {}", trend);
    }

    #[test]
    fn adx_no_trend_is_low() {
        // 窄幅震荡 → ADX 应明显低于强趋势
        let mut h = Vec::new();
        let mut l = Vec::new();
        let mut c = Vec::new();
        let mut price = 100.0;
        for i in 0..60 {
            let wiggle = if i % 2 == 0 { 0.05 } else { -0.05 };
            price += wiggle;
            h.push(price + 0.02);
            l.push(price - 0.02);
            c.push(price);
        }
        let range = adx(&h, &l, &c, 14).expect("should compute");
        assert!(range < 20.0, "range ADX too high: {}", range);
    }
}
