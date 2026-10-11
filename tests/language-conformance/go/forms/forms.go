package forms

import (
	"context"
	"iter"
)

// Square is a function bound at package level.
var Square = func(value int) int {
	return value * value
}

// CountUp yields the values below limit.
func CountUp(limit int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for value := 0; value < limit; value++ {
			if !yield(value) {
				return
			}
		}
	}
}

// Point holds two coordinates.
type Point struct {
	X int
	Y int `json:"y"`
}

// Fetch returns the URL it was given while the context lives.
func Fetch(ctx context.Context, url string) (string, error) {
	return url, ctx.Err()
}
