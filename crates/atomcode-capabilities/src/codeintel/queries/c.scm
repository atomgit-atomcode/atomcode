; C: functions, structs, enums, typedefs

(function_definition
  declarator: (function_declarator
    declarator: (identifier) @name)) @definition

(struct_specifier
  name: (type_identifier) @name) @definition

(enum_specifier
  name: (type_identifier) @name) @definition

(type_definition
  declarator: (type_identifier) @name) @definition
