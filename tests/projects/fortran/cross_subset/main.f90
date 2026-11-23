program sub
  use fbase_mod
  use fpp_mod
  implicit none
  if (base_value() /= 7) stop 1
  if (pp_value() /= 3) stop 2
end program sub
