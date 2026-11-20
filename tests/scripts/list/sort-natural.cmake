set(l f10.txt;f2.txt;f1.txt)
list(SORT l COMPARE NATURAL)
message(STATUS "l=${l}")
