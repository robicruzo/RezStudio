; Adapted from r-lib/tree-sitter-r (MIT). Later patterns take precedence.

(identifier) @variable

; Literals

(integer) @number
(float) @number
(complex) @number

(string) @string
(escape_sequence) @string.escape

(comment) @comment

; roxygen2 documentation comments
((comment) @comment.doc
  (#match? @comment.doc "^#'"))

[
  (true)
  (false)
] @boolean

[
  (null)
  (inf)
  (nan)
  (na)
] @constant.builtin

[
  (dots)
  (dot_dot_i)
] @variable.special

; Operators

[
  "?" ":=" "=" "<-" "<<-" "->" "->>"
  "~" "|>" "||" "|" "&&" "&"
  "<" "<=" ">" ">=" "==" "!="
  "+" "-" "*" "/" "::" ":::"
  "**" "^" "$" "@" ":" "!"
  "special"
] @operator

; Punctuation

[
  "(" ")"
  "{" "}"
  "[" "]"
  "[[" "]]"
] @punctuation.bracket

(comma) @punctuation.delimiter

; Members and parameters

(extract_operator
  rhs: (identifier) @property)

(parameter
  name: (identifier) @variable.parameter)

(argument
  name: (identifier) @variable.parameter)

(namespace_operator
  lhs: (identifier) @namespace)

; Functions

(binary_operator
  lhs: (identifier) @function
  operator: ["<-" "<<-" "="]
  rhs: (function_definition))

(binary_operator
  lhs: (function_definition)
  operator: ["->" "->>"]
  rhs: (identifier) @function)

(call
  function: (identifier) @function)

(call
  function: (namespace_operator
    rhs: (identifier) @function))

(call
  function: (extract_operator
    rhs: (identifier) @function))

; Keywords

(function_definition
  name: "function" @keyword)

(function_definition
  name: "\\" @keyword)

[
  "if"
  "else"
  "for"
  "in"
  "while"
  "repeat"
  (next)
  (break)
] @keyword

; `return` is an ordinary call in R, but reads as control flow.
((call
  function: (identifier) @keyword)
  (#eq? @keyword "return"))
