def parse_count(value):
    if not isinstance(value, str) or not value.strip() or not value.strip().isdigit(): raise ValueError("invalid")
    return int(value.strip())
def parse_limit(value):
    if not isinstance(value, str) or not value.strip() or not value.strip().isdigit(): raise ValueError("invalid")
    return int(value.strip())
