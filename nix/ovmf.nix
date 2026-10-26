# Secure Boot support with no keys enrolled.
{ OVMF }:

(OVMF.override {
  secureBoot = true;
  tpmSupport = true;
}).fd
