class Runtime:
    def __init__(self): self.value = 0
    def set_value(self, value): self.value = value
class Driver:
    def __init__(self, runtime): self.runtime = runtime
    def update(self, value): self.runtime.value = value
