#![allow(dead_code)]
//! Do Not Disturb during work sessions (implemented in milestone 8).

pub trait Dnd: Send {
    fn name(&self) -> &'static str;
    fn supported(&self) -> bool;
    fn enable(&mut self) -> Result<(), String>;
    fn restore(&mut self) -> Result<(), String>;
}

pub struct NoDnd;
impl Dnd for NoDnd {
    fn name(&self) -> &'static str {
        "none"
    }
    fn supported(&self) -> bool {
        false
    }
    fn enable(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn restore(&mut self) -> Result<(), String> {
        Ok(())
    }
}

pub fn detect() -> Box<dyn Dnd> {
    Box::new(NoDnd)
}
