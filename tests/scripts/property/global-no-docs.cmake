get_property(a GLOBAL PROPERTY k DEFINED)
get_property(b GLOBAL PROPERTY k BRIEF_DOCS)
get_property(c GLOBAL PROPERTY k FULL_DOCS)
message("${a}|${b}|${c}")
