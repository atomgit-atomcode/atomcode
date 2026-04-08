package main

import "testing"

func TestAddTagOnFreshUser(t *testing.T) {
	u := NewUser("alice")
	AddTag(u, "role", "admin")
	if u.Tags["role"] != "admin" {
		t.Fatalf("expected role=admin, got %q", u.Tags["role"])
	}
}

func TestEmailDomainNilEmail(t *testing.T) {
	u := NewUser("bob")
	if got := EmailDomain(u); got != "" {
		t.Fatalf("nil email should return empty string, got %q", got)
	}
}

func TestEmailDomainWithEmail(t *testing.T) {
	u := NewUser("carol")
	e := "carol@example.com"
	u.Email = &e
	if got := EmailDomain(u); got != "example.com" {
		t.Fatalf("expected example.com, got %q", got)
	}
}
