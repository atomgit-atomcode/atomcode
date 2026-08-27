from config import load_config
def configured_region(values, environ): return load_config(values).get("region", "local")
