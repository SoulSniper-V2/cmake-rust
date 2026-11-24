# Literal non-ASCII format bytes are copied unchanged around the fields.
string(TIMESTAMP t "é %Y ü %q ö" UTC)
string(REGEX REPLACE "[0-9]" "" t "${t}")
message("${t}")
