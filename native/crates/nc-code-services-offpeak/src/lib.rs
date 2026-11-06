//! Off-peak task service disabled (ZCode services removed).

pub mod disabled;

pub use disabled::DisabledOffPeakTaskService;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_compiles() {
        assert!(true);
    }
}
