//! 阻尼弹簧：灵动岛形变手感的核心。
//! 参数用 iOS 的说法：response（周期，秒）和 damping ratio（阻尼比，<1 会回弹）。

use std::f32::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    stiffness: f32,
    damping: f32,
}

impl Spring {
    pub fn new(value: f32, response: f32, damping_ratio: f32) -> Self {
        let omega = 2.0 * PI / response;
        Self {
            value,
            velocity: 0.0,
            target: value,
            stiffness: omega * omega,
            damping: 2.0 * damping_ratio * omega,
        }
    }

    /// 半隐式欧拉，固定小步长积分，帧率波动时也稳定。
    pub fn step(&mut self, dt: f32) {
        const H: f32 = 1.0 / 240.0;
        let mut t = dt.min(0.05);
        while t > 0.0 {
            let h = t.min(H);
            let accel = -self.stiffness * (self.value - self.target) - self.damping * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
            t -= h;
        }
        if self.settled() {
            self.value = self.target;
            self.velocity = 0.0;
        }
    }

    pub fn settled(&self) -> bool {
        (self.value - self.target).abs() < 0.02 && self.velocity.abs() < 0.02
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_to_target() {
        let mut s = Spring::new(0.0, 0.4, 0.8);
        s.target = 100.0;
        for _ in 0..240 {
            s.step(1.0 / 60.0);
        }
        assert!(s.settled());
        assert_eq!(s.value, 100.0);
    }

    #[test]
    fn underdamped_overshoots() {
        let mut s = Spring::new(0.0, 0.4, 0.6);
        s.target = 100.0;
        let mut max = 0.0f32;
        for _ in 0..120 {
            s.step(1.0 / 60.0);
            max = max.max(s.value);
        }
        assert!(max > 100.0, "阻尼比 < 1 应该有回弹");
    }

    #[test]
    fn huge_dt_is_clamped() {
        let mut s = Spring::new(0.0, 0.4, 0.8);
        s.target = 10.0;
        s.step(10.0);
        assert!(s.value.is_finite());
    }
}
