source_group("Sub Own" FILES t.c)
add_library(sublib STATIC s.c t.c ../x.h ../a.h)
