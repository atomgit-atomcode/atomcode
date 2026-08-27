def parse_value(raw): return raw["value"]
def handle(raw):
    from legacy import import_value
    return import_value({"old_value": raw.get("value")})
