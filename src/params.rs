use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub const MIN_GAIN_DB: f32 = -24.0;
pub const MAX_GAIN_DB: f32 = 24.0;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ParamId {
    InputGain,
    OutputGain,
    Bypass,
    Oversampling,
}

impl ParamId {
    pub const fn label(self) -> &'static str {
        match self {
            Self::InputGain => "INPUT",
            Self::OutputGain => "OUTPUT",
            Self::Bypass => "BYPASS",
            Self::Oversampling => "OS",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ParameterSnapshot {
    pub input_gain_db: f32,
    pub output_gain_db: f32,
    pub bypass: bool,
}

pub struct Parameters {
    input_gain_db: AtomicF32,
    output_gain_db: AtomicF32,
    bypass: AtomicBool,
}

impl Default for Parameters {
    fn default() -> Self {
        Self {
            input_gain_db: AtomicF32::new(0.0),
            output_gain_db: AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
        }
    }
}

impl Parameters {
    pub fn snapshot(&self) -> ParameterSnapshot {
        ParameterSnapshot {
            input_gain_db: self.input_gain_db.load(),
            output_gain_db: self.output_gain_db.load(),
            bypass: self.bypass.load(Ordering::Relaxed),
        }
    }

    pub fn reset_for_model(&self) {
        self.input_gain_db.store(0.0);
        self.output_gain_db.store(0.0);
        self.bypass.store(false, Ordering::Relaxed);
    }

    pub fn adjust_db(&self, parameter: ParamId, delta_db: f32) {
        let target = match parameter {
            ParamId::InputGain => &self.input_gain_db,
            ParamId::OutputGain => &self.output_gain_db,
            ParamId::Bypass | ParamId::Oversampling => return,
        };
        target.update(|value| (value + delta_db).clamp(MIN_GAIN_DB, MAX_GAIN_DB));
    }

    pub fn toggle_bypass(&self) {
        self.bypass.fetch_xor(true, Ordering::Relaxed);
    }

    pub fn set_normalized(&self, parameter: ParamId, value: f32) {
        let value = value.clamp(0.0, 1.0);
        match parameter {
            ParamId::InputGain => self
                .input_gain_db
                .store(MIN_GAIN_DB + value * (MAX_GAIN_DB - MIN_GAIN_DB)),
            ParamId::OutputGain => self
                .output_gain_db
                .store(MIN_GAIN_DB + value * (MAX_GAIN_DB - MIN_GAIN_DB)),
            ParamId::Bypass => self.bypass.store(value >= 0.5, Ordering::Relaxed),
            ParamId::Oversampling => {}
        }
    }

    pub fn normalized(&self, parameter: ParamId) -> f32 {
        let snapshot = self.snapshot();
        match parameter {
            ParamId::InputGain => normalize_gain(snapshot.input_gain_db),
            ParamId::OutputGain => normalize_gain(snapshot.output_gain_db),
            ParamId::Bypass => {
                if snapshot.bypass {
                    1.0
                } else {
                    0.0
                }
            }
            ParamId::Oversampling => 0.0,
        }
    }

    pub fn indicator_delta(&self, parameter: ParamId) -> f32 {
        let original = match parameter {
            ParamId::InputGain | ParamId::OutputGain => 0.5,
            ParamId::Bypass | ParamId::Oversampling => 0.0,
        };
        self.normalized(parameter) - original
    }
}

fn normalize_gain(value: f32) -> f32 {
    ((value - MIN_GAIN_DB) / (MAX_GAIN_DB - MIN_GAIN_DB)).clamp(0.0, 1.0)
}

struct AtomicF32(AtomicU32);

impl AtomicF32 {
    fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    fn update(&self, update: impl Fn(f32) -> f32) {
        let mut old = self.0.load(Ordering::Relaxed);
        loop {
            let new = update(f32::from_bits(old)).to_bits();
            match self
                .0
                .compare_exchange_weak(old, new, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(observed) => old = observed,
            }
        }
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}
