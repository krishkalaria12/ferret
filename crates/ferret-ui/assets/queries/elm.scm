; elm highlights for Ferret's preview pane.
; Vendored from tree-sitter-elm 5.9.4 (MIT), queries/highlights.scm
; https://github.com/elm-tooling/tree-sitter-elm
; 41-slot adaptations: @local.function.elm -> @variable (value refs and
; patterns), @meta.import.elm -> @keyword.import, @storage.type.elm -> @type,
; @union.elm -> @constructor, @char.elm -> @string (quote marks),
; @character.escape.elm -> @string.escape, @source.glsl -> @embedded.
; Keywords
[
    "if"
    "then"
    "else"
    "let"
    "in"
 ] @keyword.control.elm
(case) @keyword.control.elm
(of) @keyword.control.elm

(colon) @keyword.other.elm
(backslash) @keyword.other.elm
(as) @keyword.other.elm
(port) @keyword.other.elm
(exposing) @keyword.other.elm
(alias) @keyword.other.elm
(infix) @keyword.other.elm

(arrow) @keyword.operator.arrow.elm

(port) @keyword.other.port.elm

(type_annotation(lower_case_identifier) @function.elm)
(port_annotation(lower_case_identifier) @function.elm)
(function_declaration_left(lower_case_identifier) @function.elm)
(function_call_expr target: (value_expr) @function.elm)

(field_access_expr(value_expr(value_qid)) @variable)
(lower_pattern) @variable
(record_base_identifier) @variable


(operator_identifier) @keyword.operator.elm
(eq) @keyword.operator.assignment.elm


"(" @punctuation.section.braces
")" @punctuation.section.braces

"|" @keyword.other.elm
"," @punctuation.separator.comma.elm

(import) @keyword.import
(module) @keyword.other.elm

(number_constant_expr) @constant.numeric.elm


(type) @keyword.type.elm

(type_declaration(upper_case_identifier) @type)
(type_ref) @type
(type_alias_declaration name: (upper_case_identifier) @type)

(union_variant(upper_case_identifier) @constructor)
(union_pattern) @constructor
(value_expr(upper_case_qid(upper_case_identifier)) @constructor)

; comments
(line_comment) @comment.elm
(block_comment) @comment.elm

; strings
(string_escape) @string.escape

(open_quote) @string.elm
(close_quote) @string.elm
(regular_string_part) @string.elm

(open_char) @string
(close_char) @string


; glsl
(glsl_content) @embedded
