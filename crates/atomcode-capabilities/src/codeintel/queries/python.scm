; Python: functions, classes, methods, decorators

(function_definition
  name: (identifier) @name) @definition

(class_definition
  name: (identifier) @name) @definition

(decorated_definition
  definition: (function_definition
    name: (identifier) @name)) @definition

(decorated_definition
  definition: (class_definition
    name: (identifier) @name)) @definition
