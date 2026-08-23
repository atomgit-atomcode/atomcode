USERS = set()
def create_user(name):
    normalized = name.strip().lower()
    if normalized in USERS: return False
    USERS.add(normalized)
    return True
