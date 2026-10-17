# Example fresh-install worker. Copy this directory to nix/hosts/<name>/, edit the values,
# then install with nixos-anywhere (nix/README.md "Fresh install"); THAT WIPES `disk`.
# `nixosConfigurations.<name>` is created for every directory here that has a default.nix.
# The hardware report nix/hosts/<name>/facter.json is written by
# `nixos-anywhere --generate-hardware-config nixos-facter …`; until it exists the
# configuration evaluates without it (generic hardware).
{
  tasksWorkerHost = {
    hostName = "worker-example";
    address = "192.168.1.51"; # fixed by a DHCP reservation
    interface = null; # e.g. "enp3s0" to open the peer port on that interface only
    disk = "/dev/nvme0n1"; # check with `lsblk` on the target first: it is ERASED
    controller = {
      name = "laptop";
      # The laptop's `tasks peer id`. Synthetic example value; replace it.
      endpointId = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    };
    # Your real ssh public key(s). Synthetic example value; replace it.
    sshAuthorizedKeys = [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExampleExampleExampleExampleExampleExample0 example"
    ];
  };
}
