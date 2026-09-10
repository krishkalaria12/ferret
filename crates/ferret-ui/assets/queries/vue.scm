; vue highlights for Ferret’s preview pane.
; Vendored from tree-sitter-vue-next 0.1.0 (MIT):
; queries/html_tags/highlights.scm + queries/vue/highlights.scm
; https://github.com/jamie8johnson/tree-sitter-vue
; Upstream layers vue over html_tags via `; inherits:`; gpui-component has
; no inheritance, so the two files are flattened here with html_tags first
; (later patterns win ties, matching upstream precedence).
; 41-slot adaptations: @markup.heading[.1-6] -> @title;
; @markup.strong -> @emphasis.strong; @markup.italic -> @emphasis;
; @markup.strikethrough/@markup.underline -> @none (no strike/underline
; slots); @markup.raw -> @text.code.span; @markup.link.label -> @link_text;
; @character.special -> @string.special. `@click` appears only in prose.
(tag_name) @tag @nospell

; (erroneous_end_tag_name) @error ; we do not lint syntax errors
(comment) @comment @spell

(attribute_name) @tag.attribute @nospell

(attribute_value) @nospell

((attribute
  (quoted_attribute_value) @string)
  (#set! priority 99))

(text) @none @spell

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "title"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "h1"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "h2"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "h3"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "h4"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "h5"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @title)
  (#eq? @_tag "h6"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @emphasis.strong)
  (#any-of? @_tag "strong" "b"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @emphasis)
  (#any-of? @_tag "em" "i"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @none)
  (#any-of? @_tag "s" "del"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @none)
  (#eq? @_tag "u"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @text.code.span)
  (#any-of? @_tag "code" "kbd"))

((element
  (start_tag
    (tag_name) @_tag)
  (text) @link_text)
  (#eq? @_tag "a"))

((attribute
  (attribute_name) @_attr
  (quoted_attribute_value
    (attribute_value) @string.special.url))
  (#any-of? @_attr "href" "src"))

[
  "<"
  ">"
  "</"
  "/>"
] @tag.delimiter

"=" @operator

; ---- vue additions (flattened; upstream inherits html_tags) ----

[
  "["
  "]"
] @punctuation.bracket

[
  ":"
  "."
] @string.special

[
  (interpolation)
  "@"
] @punctuation.special

(interpolation
  (raw_text) @none)

(dynamic_directive_inner_value) @variable

(directive_name) @tag.attribute

; Accessing a component object's field
(":"
  .
  (directive_value) @variable.member)

("."
  .
  (directive_value) @property)

; @click is like onclick for HTML
("@"
  .
  (directive_value) @function.method)

; Used in v-slot, declaring position the element should be put in
("#"
  .
  (directive_value) @variable)

(directive_attribute
  (quoted_attribute_value) @punctuation.special)

(directive_attribute
  (quoted_attribute_value
    (attribute_value) @none))

(directive_modifier) @function.method

((template_element) @_template
  (#set! @_template bo.commentstring "<!-- %s -->"))
