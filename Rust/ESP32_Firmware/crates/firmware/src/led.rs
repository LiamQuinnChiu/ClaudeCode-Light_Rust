//! 三路 LED PWM 驱动与灯效动画任务。
//!
//! 公共正极接法：占空比 = 255 - 亮度。红灯上限 255、黄绿 220。
//! 8 位分辨率、5kHz。

use alloc::boxed::Box;

use embassy_time::{Duration, Instant};
use esp_hal::{
    gpio::{DriveMode, interconnect::PeripheralOutput},
    ledc::{
        Ledc, LowSpeed, LSGlobalClkSource,
        channel::{self, Channel, ChannelHW, ChannelIFace},
        timer::{self, Timer, TimerIFace},
    },
    peripherals::LEDC,
    time::Rate,
};
use log::info;

use light_core::effects::{self, GREEN_MAX, RED_MAX, Rgb, YELLOW_MAX};
use light_core::modes::Mode;

use crate::LedChannel;

const PWM_FREQ_HZ: u32 = 5000;

/// 三路 PWM 通道：IO4 红、IO3 黄、IO2 绿。
pub struct Leds {
    red: &'static mut Channel<'static, LowSpeed>,
    yellow: &'static mut Channel<'static, LowSpeed>,
    green: &'static mut Channel<'static, LowSpeed>,
}

impl Leds {
    pub fn new(
        ledc: LEDC<'static>,
        green_pin: impl PeripheralOutput<'static>,
        yellow_pin: impl PeripheralOutput<'static>,
        red_pin: impl PeripheralOutput<'static>,
    ) -> Self {
        let mut ledc = Ledc::new(ledc);

        // 分频按 APB 时钟计算，必须先选定时钟源，否则定时器不按预期计数
        ledc.set_global_slow_clock(LSGlobalClkSource::APBClk);

        // 定时器与通道需要 'static 生命周期
        let timer: &'static mut Timer<'static, LowSpeed> =
            Box::leak(Box::new(ledc.timer::<LowSpeed>(timer::Number::Timer0)));

        timer
            .configure(timer::config::Config {
                duty: timer::config::Duty::Duty8Bit,
                clock_source: timer::LSClockSource::APBClk,
                frequency: Rate::from_hz(PWM_FREQ_HZ),
            })
            .expect("LEDC timer configure failed");

        let timer_ref: &'static dyn TimerIFace<LowSpeed> = timer;

        let red = Box::leak(Box::new(
            ledc.channel::<LowSpeed>(channel::Number::Channel0, red_pin),
        ));
        let yellow = Box::leak(Box::new(
            ledc.channel::<LowSpeed>(channel::Number::Channel1, yellow_pin),
        ));
        let green = Box::leak(Box::new(
            ledc.channel::<LowSpeed>(channel::Number::Channel2, green_pin),
        ));

        for ch in [&mut *red, &mut *yellow, &mut *green] {
            // 初始 100% 占空比 = 熄灭
            ch.configure(channel::config::Config {
                timer: timer_ref,
                duty_pct: 100,
                drive_mode: DriveMode::PushPull,
            })
            .expect("LEDC channel configure failed");
        }

        let mut leds = Leds { red, yellow, green };
        // 确保初始状态为 OFF，避免开机时 LED 亮度未定义
        leds.write(Rgb::OFF);
        leds
    }


    /// 写入目标亮度：限幅后反相输出。
    pub fn write(&mut self, rgb: Rgb) {
        self.red.set_duty_hw(u32::from(255 - rgb.red.min(RED_MAX)));
        self.yellow
            .set_duty_hw(u32::from(255 - rgb.yellow.min(YELLOW_MAX)));
        self.green
            .set_duty_hw(u32::from(255 - rgb.green.min(GREEN_MAX)));
    }
}

/// BLE 任务发给动画任务的消息。
pub enum LedCmd {
    /// 切换模式，同时重置空闲计时。
    Mode(Mode),
    /// OTA 期间挂起动画。
    OtaSuspend,
    /// OTA 结束，恢复动画。
    OtaResume,
}

const TICK_MS: u64 = 5;
const IDLE_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const FADE_STEPS: u32 = 12;
const FADE_STEP_MS: u64 = 7;

/// 切到红/黄/绿时的非阻塞淡入。
struct Fade {
    target: Rgb,
    step: u32,
    last: Instant,
}

impl Fade {
    fn new(target: Rgb, now: Instant) -> Self {
        Fade { target, step: 0, last: now }
    }

    fn tick(&mut self, now: Instant) -> Rgb {
        if self.step < FADE_STEPS && (now - self.last).as_millis() >= FADE_STEP_MS {
            self.step += 1;
            self.last = now;
        }
        let p = self.step;
        Rgb::new(
            (u32::from(self.target.red) * p / FADE_STEPS) as u8,
            (u32::from(self.target.yellow) * p / FADE_STEPS) as u8,
            (u32::from(self.target.green) * p / FADE_STEPS) as u8,
        )
    }

    fn done(&self) -> bool {
        self.step >= FADE_STEPS
    }
}

/// 灯效动画任务：唯一操作 PWM 的任务。
#[embassy_executor::task]
pub async fn led_task(mut leds: Leds, channel: &'static LedChannel) {
    let boot = Instant::now();
    let mut mode = Mode::Demo;
    let mut mode_start = boot;
    let mut last_write = boot;
    let mut fade: Option<Fade> = None;
    let mut suspended = false;
    let mut first = true;

    loop {
        while let Ok(cmd) = channel.try_receive() {
            match cmd {
                LedCmd::Mode(m) => {
                    let now = Instant::now();
                    mode = m;
                    mode_start = now;
                    last_write = now;
                    fade = match m {
                        Mode::Red => Some(Fade::new(Rgb::new(RED_MAX, 0, 0), now)),
                        Mode::Yellow => Some(Fade::new(Rgb::new(0, YELLOW_MAX, 0), now)),
                        Mode::Green => Some(Fade::new(Rgb::new(0, 0, GREEN_MAX), now)),
                        _ => None,
                    };
                    info!("Mode changed to: {}", m.as_str());
                }
                LedCmd::OtaSuspend => suspended = true,
                LedCmd::OtaResume => suspended = false,
            }
        }

        if !suspended {
            let now = Instant::now();

            if mode != Mode::Off && (now - last_write).as_millis() >= IDLE_TIMEOUT_MS {
                info!("Idle timeout ({}) -> off", mode.as_str());
                mode = Mode::Off;
                mode_start = now;
                fade = None;
            }

            let mut fade_finished = false;
            let rgb = if let Some(f) = fade.as_mut() {
                let v = f.tick(now);
                fade_finished = f.done();
                v
            } else {
                let t_rel = (now - mode_start).as_millis() as u32;
                let uptime = (now - boot).as_millis() as u32;
                effects::effect(mode, t_rel, uptime)
            };
            if fade_finished {
                fade = None;
            }

            if first {
                info!(
                    "LED ready: r={} y={} g={}",
                    rgb.red, rgb.yellow, rgb.green
                );
                first = false;
            }
            leds.write(rgb);
        }

        embassy_time::Timer::after(Duration::from_millis(TICK_MS)).await;
    }
}
