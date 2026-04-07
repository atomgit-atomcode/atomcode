package main

type User struct {
	Name  string
	Tags  map[string]string // BUG: may be nil when User is constructed from NewUser
	Email *string           // BUG: caller may not set this, dereferencing crashes
}

// NewUser creates a User but forgets to initialize the Tags map.
func NewUser(name string) *User {
	return &User{Name: name}
}

// AddTag panics when Tags is nil.
func AddTag(u *User, k, v string) {
	u.Tags[k] = v
}

// EmailDomain returns the domain part after '@', or "" if no email.
// Currently panics when Email is nil.
func EmailDomain(u *User) string {
	s := *u.Email
	for i := 0; i < len(s); i++ {
		if s[i] == '@' {
			return s[i+1:]
		}
	}
	return ""
}

func main() {}
