("(" @open
  ")" @close)

("[" @open
  "]" @close)

("[[" @open
  "]]" @close)

("{" @open
  "}" @close)

(((string_open) @open
  (string_close) @close)
  (#set! rainbow.exclude))
