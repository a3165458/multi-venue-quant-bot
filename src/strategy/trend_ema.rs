/// 计算 EMA 序列的最后两个值 (prev, current)
pub(super) fn ema_last_two(prices: &[f64], period: usize) -> Option<(f64, f64)> {
    if prices.len() < period + 1 {
        return None;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    // 用前 period 根的 SMA 作为 EMA 种子
    let seed: f64 = prices[..period].iter().sum::<f64>() / period as f64;
    let mut ema = seed;
    let mut prev = seed;
    for &p in &prices[period..] {
        prev = ema;
        ema = alpha * p + (1.0 - alpha) * ema;
    }
    Some((prev, ema))
}
