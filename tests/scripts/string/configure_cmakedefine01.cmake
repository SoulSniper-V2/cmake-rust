set(D1 "yes")
set(D2 "NO")
set(D3 "")
string(CONFIGURE "#cmakedefine01 D1
#cmakedefine01 D2
#cmakedefine01 D3
#cmakedefine01 D4
# cmakedefine01 D1" out)
message("<${out}>")
