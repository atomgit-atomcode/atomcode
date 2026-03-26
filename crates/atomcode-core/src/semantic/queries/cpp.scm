; C++: functions, classes, structs, methods, namespaces, templates

(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition

(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier) @name)) @definition

(class_specifier
  name: (type_identifier) @name) @definition

(struct_specifier
  name: (type_identifier) @name) @definition

(namespace_definition
  name: (identifier) @name) @definition

(enum_specifier
  name: (type_identifier) @name) @definition
