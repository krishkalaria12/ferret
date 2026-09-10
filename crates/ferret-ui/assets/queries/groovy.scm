; groovy highlights for Ferret’s preview pane.
; Hand-written for tree-sitter-groovy 0.1.2 (MIT)
; https://github.com/amaanq/tree-sitter-groovy
; nvim-treesitter's groovy queries target a newer grammar generation (22
; unknown nodes, 14 unknown operators), so this covers the core surface
; instead. Field names verified against the pinned grammar's node-types.json.
; 41-slot adaptations: none — written directly in the 41-slot vocabulary.

(line_comment) @comment @spell

(block_comment) @comment

(shebang) @comment

[
  (decimal_integer_literal)
  (hex_integer_literal)
  (octal_integer_literal)
  (binary_integer_literal)
  (decimal_floating_point_literal)
  (hex_floating_point_literal)
] @number

[
  (true)
  (false)
] @boolean

(null_literal) @constant.builtin

(this) @variable.builtin

(string_literal) @string

(character_literal) @string

(escape_sequence) @string.escape

(method_declaration
  name: (identifier) @function.method)

(function_definition
  name: (identifier) @function)

(method_invocation
  name: (identifier) @function.call)

(class_declaration
  name: (_) @type)

(interface_declaration
  name: (_) @type)

(formal_parameter
  name: (_) @variable.parameter)

(identifier) @variable

[
  "class"
  "interface"
  "def"
  "import"
  "package"
  "return"
  "if"
  "else"
  "for"
  "while"
  "try"
  "catch"
  "finally"
  "new"
] @keyword
