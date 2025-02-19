pub struct Signal {
    pub name: String,
    pub value: f64,
}

impl Signal {
    pub fn new(name: &str, value: f64) -> Self {
        Signal {
            name: name.to_string(),
            value,
        }
    }

    pub fn should_buy(&self) -> bool {
        self.value > 0.0
    }

    pub fn should_sell(&self) -> bool {
        self.value < 0.0
    }
}

pub fn process_signal(signal: &Signal) {
    if signal.should_buy() {
        println!("Executing buy action for signal: {}", signal.name);
        // Add buy logic here
    } else if signal.should_sell() {
        println!("Executing sell action for signal: {}", signal.name);
        // Add sell logic here
    } else {
        println!("No action for signal: {}", signal.name);
    }
}
