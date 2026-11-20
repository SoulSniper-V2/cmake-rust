set(D1 "yes")
set(D2 "NO")
set(D3 "")
string(CONFIGURE "#cmakedefine D1
#cmakedefine D2
#cmakedefine D3
#cmakedefine D4
#  cmakedefine D1 foo
#define inline
#cmakedefine D1 \"quoted\"
not a define #cmakedefine D1" out)
message("<${out}>")
