program main
  use, non_intrinsic :: &
    parent, only: answer
  if (answer() /= 42) stop 1
end program
