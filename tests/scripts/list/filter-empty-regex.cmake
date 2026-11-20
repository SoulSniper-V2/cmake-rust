set(l a;b)
list(FILTER l INCLUDE REGEX "")
message(STATUS "l='${l}'")
