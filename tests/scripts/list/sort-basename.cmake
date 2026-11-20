set(l /x/b.txt;/y/a.txt;z/c.txt)
list(SORT l COMPARE FILE_BASENAME)
message(STATUS "l=${l}")
