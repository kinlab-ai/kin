package ledger

import "example.com/shop/helper"

func clamp(value int) int {
	if value < 0 {
		return 0
	}
	return value
}

// Total sums the values, scaled and clamped.
func Total(values []int) int {
	double := func(x int) int {
		return x * 2
	}
	result := 0
	for _, value := range values {
		result += clamp(helper.Scale(double(value), 2))
	}
	return result
}

// Account is a running balance.
type Account struct {
	balance int
}

// Deposit adds a valid amount.
func (a *Account) Deposit(amount int) {
	if a.validate(amount) {
		a.balance += amount
	}
}

func (a *Account) validate(amount int) bool {
	return amount > 0
}
