use crate::helper::scale;

fn clamp(value: i64) -> i64 {
    value.max(0)
}

/// Sum the values, scaled and clamped.
#[must_use]
pub fn total(values: &[i64]) -> i64 {
    fn double(x: i64) -> i64 {
        x * 2
    }
    let mut result = 0;
    for value in values {
        result += clamp(scale(double(*value), 2));
    }
    result
}

/// A running balance.
#[derive(Default)]
pub struct Account {
    balance: i64,
}

impl Account {
    /// Add a valid amount.
    pub fn deposit(&mut self, amount: i64) {
        if self.validate(amount) {
            self.balance += amount;
        }
    }

    fn validate(&self, amount: i64) -> bool {
        amount > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_total() {
        assert_eq!(total(&[1, 2]), 12);
    }
}
