{ runCommand, qemu }:

runCommand "keymaps.csv" { } ''
  tar -xOf ${qemu.src} --wildcards '*/subprojects/keycodemapdb/data/keymaps.csv' > $out
''
