package calc

import "testing"

func TestAddZero(t *testing.T) {
	if got := Add(0, 0); got != 0 {
		t.Errorf("Add(0,0) = %d, want 0", got)
	}
}

func TestAddPositives(t *testing.T) {
	if got := Add(2, 3); got != 5 {
		t.Errorf("Add(2,3) = %d, want 5", got)
	}
}

func TestAddNegatives(t *testing.T) {
	if got := Add(-1, -2); got != -3 {
		t.Errorf("Add(-1,-2) = %d, want -3", got)
	}
}

func TestAddMixed(t *testing.T) {
	if got := Add(10, -4); got != 6 {
		t.Errorf("Add(10,-4) = %d, want 6", got)
	}
}

func TestAddLarge(t *testing.T) {
	if got := Add(1_000_000, 1); got != 1_000_001 {
		t.Errorf("Add(1000000,1) = %d, want 1000001", got)
	}
}
