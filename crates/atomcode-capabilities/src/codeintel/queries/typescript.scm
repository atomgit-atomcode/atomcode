; TypeScript: functions, classes, interfaces, type aliases, enums

(function_declaration
  name: (identifier) @name) @definition

(class_declaration
  name: (type_identifier) @name) @definition

(interface_declaration
  name: (type_identifier) @name) @definition

(type_alias_declaration
  name: (type_identifier) @name) @definition

(enum_declaration
  name: (identifier) @name) @definition

(method_definition
  name: (property_identifier) @name) @definition

(lexical_declaration
  (variable_declarator
    name: (identifier) @name
    value: (arrow_function))) @definition

(export_statement
  declaration: (function_declaration
    name: (identifier) @name)) @definition

(export_statement
  declaration: (class_declaration
    name: (type_identifier) @name)) @definition
