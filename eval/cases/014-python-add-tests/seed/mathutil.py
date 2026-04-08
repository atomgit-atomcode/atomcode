"""Small collection of math utilities."""


def clamp(x, lo, hi):
    """Clamp x into [lo, hi]. Raise ValueError if lo > hi."""
    if lo > hi:
        raise ValueError("lo must be <= hi")
    if x < lo:
        return lo
    if x > hi:
        return hi
    return x


def mean(xs):
    """Arithmetic mean. Raise ValueError on empty input."""
    if not xs:
        raise ValueError("mean of empty sequence")
    return sum(xs) / len(xs)


def is_prime(n):
    """Return True if n is a prime number (n >= 2)."""
    if n < 2:
        return False
    if n < 4:
        return True
    if n % 2 == 0:
        return False
    i = 3
    while i * i <= n:
        if n % i == 0:
            return False
        i += 2
    return True
