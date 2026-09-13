
use crate::modes::Mode;

pub const RED_MAX: u8 = 255;
pub const YELLOW_MAX: u8 = 220;
pub const GREEN_MAX: u8 = 220;


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub red: u8,
    pub yellow: u8,
    pub green: u8,
}

impl Rgb {
    pub const OFF: Rgb = Rgb { red: 0, yellow: 0, green: 0 };

    pub const fn new(red: u8, yellow: u8, green: u8) -> Self {
        Rgb { red, yellow, green }
    }
}

fn map(x: i64, in_min: i64, in_max: i64, out_min: i64, out_max: i64) -> i64 {
    (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min
}

fn clamp_255(v: i64) -> u8 {
    v.clamp(0, 255) as u8
}


fn tri_wave(t: u32, period: u32, max: u8) -> u8 {
    let x = t % period;
    let half = period / 2;
    if x < half {
        clamp_255(map(x as i64, 0, half as i64, 0, max as i64))
    } else {
        clamp_255(map(x as i64, half as i64, period as i64, max as i64, 0))
    }
}


fn fade_in_out(t: u32, fade_in: u32, hold: u32, fade_out: u32, off: u32, max: u8) -> u8 {
    let total = fade_in + hold + fade_out + off;
    let x = t % total;

    if x < fade_in {
        return clamp_255(map(x as i64, 0, fade_in as i64, 0, max as i64));
    }
    let x = x - fade_in;
    if x < hold {
        return max;
    }
    let x = x - hold;
    if x < fade_out {
        return clamp_255(map(x as i64, 0, fade_out as i64, max as i64, 0));
    }
    0
}

fn thinking(t: u32) -> Rgb {
    const PERIOD: u32 = 2400;
    let g = fade_in_out(t, 300, 200, 400, 300, GREEN_MAX);
    let y = fade_in_out(t + PERIOD / 2, 300, 200, 400, 300, YELLOW_MAX);
    Rgb::new(0, y, g)
}


fn ai(t: u32) -> Rgb {
    const SLOT: u32 = 600;
    let mut g = fade_in_out(t, 250, 100, 200, 50, 120);
    let y = fade_in_out(t + SLOT, 250, 100, 200, 50, 110);
    let r = fade_in_out(t + 2 * SLOT, 250, 100, 200, 50, 140);
    if r > 0 {
        g = 0;
    }
    Rgb::new(r, y, g)
}


fn error(t: u32) -> Rgb {
    let r = fade_in_out(t, 40, 180, 80, 180, RED_MAX);
    Rgb::new(r, 0, 0)
}


fn alarm(t: u32) -> Rgb {
    const PHASE: u32 = 260;
    let phase = (t / PHASE) % 2;
    let inside = t % PHASE;

    let b = if inside < 60 {
        map(inside as i64, 0, 60, 0, 255)
    } else if inside < 180 {
        255
    } else {
        map(inside as i64, 180, PHASE as i64, 255, 0)
    };

    if phase == 0 {
        Rgb::new(clamp_255(b), 0, 0)
    } else {
        Rgb::new(0, clamp_255(b).min(YELLOW_MAX), 0)
    }
}


fn demo(t_rel: u32, uptime: u32) -> Rgb {
    let t = t_rel % 16000;

    if t < 1200 {
        let g = tri_wave(t, 1200, GREEN_MAX);
        Rgb::new(0, 0, g)
    } else if t < 2400 {
        let y = tri_wave(t - 1200, 1200, YELLOW_MAX);
        Rgb::new(0, y, 0)
    } else if t < 3600 {
        let r = tri_wave(t - 2400, 1200, RED_MAX);
        Rgb::new(r, 0, 0)
    } else if t < 6200 {
        ai(uptime)
    } else if t < 8200 {
        thinking(uptime)
    } else if t < 10200 {
        Rgb::new(0, YELLOW_MAX, 0)
    } else if t < 12200 {
        error(uptime)
    } else if t < 14200 {
        alarm(uptime)
    } else {
        let p = t - 14200;
        if p < 600 {
            Rgb::new(RED_MAX, 0, 0)
        } else if p < 1200 {
            Rgb::new(0, 0, GREEN_MAX)
        } else {
            Rgb::new(0, YELLOW_MAX, 0)
        }
    }
}


pub fn effect(mode: Mode, t_rel: u32, uptime: u32) -> Rgb {
    match mode {
        Mode::Demo => demo(t_rel, uptime),
        Mode::Thinking => thinking(t_rel),
        Mode::Ai => ai(t_rel),
        Mode::Busy => Rgb::new(0, YELLOW_MAX, 0),
        Mode::Success => Rgb::new(0, 0, GREEN_MAX),
        Mode::Error => error(t_rel),
        Mode::Alarm => alarm(t_rel),
        Mode::Off => Rgb::OFF,
        Mode::Red => Rgb::new(RED_MAX, 0, 0),
        Mode::Yellow => Rgb::new(0, YELLOW_MAX, 0),
        Mode::Green => Rgb::new(0, 0, GREEN_MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tri_wave_up_and_down() {
        assert_eq!(tri_wave(0, 1200, 220), 0);
        assert_eq!(tri_wave(300, 1200, 220), 110);
        assert_eq!(tri_wave(599, 1200, 220), 219);
        assert_eq!(tri_wave(600, 1200, 220), 220);
        assert_eq!(tri_wave(900, 1200, 220), 110);
        assert_eq!(tri_wave(1199, 1200, 220), 1);
        assert_eq!(tri_wave(1200, 1200, 220), 0);
    }

    #[test]
    fn fade_in_out_shape() {
        assert_eq!(fade_in_out(0, 300, 200, 400, 300, 220), 0);
        assert_eq!(fade_in_out(150, 300, 200, 400, 300, 220), 110);
        assert_eq!(fade_in_out(300, 300, 200, 400, 300, 220), 220);
        assert_eq!(fade_in_out(499, 300, 200, 400, 300, 220), 220);
        assert_eq!(fade_in_out(500, 300, 200, 400, 300, 220), 220);
        assert_eq!(fade_in_out(700, 300, 200, 400, 300, 220), 110);
        assert_eq!(fade_in_out(900, 300, 200, 400, 300, 220), 0);
        assert_eq!(fade_in_out(1100, 300, 200, 400, 300, 220), 0);
        assert_eq!(fade_in_out(1200, 300, 200, 400, 300, 220), 0);
    }

    #[test]
    fn alarm_phases() {
        assert_eq!(alarm(0), Rgb::new(0, 0, 0));
        assert_eq!(alarm(30), Rgb::new(127, 0, 0));
        assert_eq!(alarm(60), Rgb::new(255, 0, 0));
        assert_eq!(alarm(179), Rgb::new(255, 0, 0));
        assert_eq!(alarm(180), Rgb::new(255, 0, 0));
        assert_eq!(alarm(220), Rgb::new(128, 0, 0));
        assert_eq!(alarm(259), Rgb::new(4, 0, 0));
        assert_eq!(alarm(260), Rgb::new(0, 0, 0));
        assert_eq!(alarm(320), Rgb::new(0, 220, 0));
    }

    #[test]
    fn ai_red_suppresses_green() {
        assert_eq!(ai(0), Rgb::new(0, 0, 0));
        assert_eq!(ai(300), Rgb::new(140, 110, 0));
    }

    #[test]
    fn thinking_is_symmetric() {
        let rgb = thinking(700);
        assert_eq!(rgb.green, rgb.yellow);
        assert_eq!(rgb.red, 0);
    }

    #[test]
    fn error_shape() {
        assert_eq!(error(0), Rgb::new(0, 0, 0));
        assert_eq!(error(40), Rgb::new(255, 0, 0));
        assert_eq!(error(200), Rgb::new(255, 0, 0));
        assert_eq!(error(260), Rgb::new(128, 0, 0));
        assert_eq!(error(300), Rgb::new(0, 0, 0));
        assert_eq!(error(310), Rgb::new(0, 0, 0));
    }

    #[test]
    fn static_modes() {
        assert_eq!(effect(Mode::Red, 0, 0), Rgb::new(255, 0, 0));
        assert_eq!(effect(Mode::Yellow, 0, 0), Rgb::new(0, 220, 0));
        assert_eq!(effect(Mode::Green, 0, 0), Rgb::new(0, 0, 220));
        assert_eq!(effect(Mode::Busy, 0, 0), Rgb::new(0, 220, 0));
        assert_eq!(effect(Mode::Success, 0, 0), Rgb::new(0, 0, 220));
        assert_eq!(effect(Mode::Off, 12345, 999), Rgb::new(0, 0, 0));
    }

    #[test]
    fn demo_sequence() {
        assert_eq!(demo(0, 0), Rgb::new(0, 0, 0));
        assert_eq!(demo(600, 0), Rgb::new(0, 0, 220));
        assert_eq!(demo(1500, 0).yellow, 110);
        assert_eq!(demo(2700, 0).red, 127);
        assert_eq!(demo(5000, 5000), ai(5000));
        assert_eq!(demo(7000, 7000), thinking(7000));
        assert_eq!(demo(9000, 9000), Rgb::new(0, 220, 0));
        assert_eq!(demo(10250, 10250), error(10250));
        assert_eq!(demo(12300, 12300), alarm(12300));
        assert_eq!(demo(14300, 0), Rgb::new(255, 0, 0));
        assert_eq!(demo(15000, 0), Rgb::new(0, 0, 220));
        assert_eq!(demo(15600, 0), Rgb::new(0, 220, 0));
        assert_eq!(demo(16000, 0), demo(0, 0));
    }
}
