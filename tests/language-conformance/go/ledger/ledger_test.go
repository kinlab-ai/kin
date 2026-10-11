package ledger

import "testing"

func TestTotal(t *testing.T) {
	if got := Total([]int{1, 2}); got != 12 {
		t.Fatalf("Total = %d, want 12", got)
	}
}
