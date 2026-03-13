# Setting Up Arch Linux on D: Drive — For Idiots

You have Windows on C:, an empty 1TB D: drive, and you want to dual-boot Arch Linux.
This guide assumes you know nothing. Every step is spelled out.

## What You Need

- A USB drive (8GB+)
- Your empty 1TB D: drive
- An internet connection (ethernet is easiest, WiFi works too)
- About 1-2 hours
- This guide open on your phone or another computer (you won't have a browser during install)

## Phase 0: Prep From Windows (10 minutes)

### Download the Arch ISO
1. Go to https://archlinux.org/download/
2. Scroll to your country, click any mirror
3. Download the `.iso` file (about 1GB)

### Flash it to USB
1. Download Rufus: https://rufus.ie
2. Plug in your USB drive
3. Open Rufus:
   - Device: your USB drive
   - Boot selection: click SELECT, pick the Arch .iso
   - Partition scheme: **GPT**
   - Target system: **UEFI**
   - Click START, click OK on any warnings
4. Wait for it to finish

### Check your D: drive
1. Open Disk Management (right-click Start → Disk Management)
2. Find your 1TB drive — it should show as unallocated or a single partition
3. If it has partitions, right-click each → Delete Volume (THIS ERASES EVERYTHING ON D:)
4. Leave it as unallocated — the Arch installer will handle it
5. Note which disk number it is (Disk 0, Disk 1, etc.) — you'll need this

### Disable Fast Startup (IMPORTANT)
Windows Fast Startup can corrupt your Linux partition.
1. Control Panel → Power Options → "Choose what the power buttons do"
2. Click "Change settings that are currently unavailable"
3. UNCHECK "Turn on fast startup"
4. Save changes

### Note your UEFI/BIOS key
Usually F2, F12, DEL, or ESC at boot. Check your motherboard manual.

## Phase 1: Boot the USB (5 minutes)

1. Restart your PC
2. Mash your BIOS key during boot
3. In BIOS:
   - Find Boot Order / Boot Menu
   - Set USB drive as #1 boot device
   - Make sure **UEFI mode** is enabled (not Legacy/CSM)
   - Save and exit
4. PC reboots into the Arch installer
5. Select "Arch Linux install medium" and press Enter
6. You'll land at a `root@archiso` prompt — you're in

## Phase 2: Connect to Internet (2 minutes)

### If ethernet (recommended)
It probably just works. Test it:
```bash
ping -c 3 archlinux.org
```
If you see replies, skip to Phase 3.

### If WiFi
```bash
iwctl
```
Inside iwctl:
```
station wlan0 scan
station wlan0 get-networks
station wlan0 connect "Your WiFi Name"
```
Type your password, then `exit`. Test with `ping`.

## Phase 3: Find Your D: Drive (5 minutes)

This is the scariest part. You need to identify the RIGHT drive.

```bash
lsblk
```

This shows all drives. Example output:
```
NAME   SIZE TYPE
sda    500G disk     ← This is probably your C: drive (Windows)
├─sda1 100M part     ← EFI partition
├─sda2  16M part     ← Microsoft reserved
└─sda3 499G part     ← Windows C:
sdb    1TB  disk     ← This is probably your D: drive (EMPTY)
nvme0n1 500G disk    ← Or drives might be nvme
```

**HOW TO IDENTIFY YOUR D: DRIVE:**
- It's the 1TB one
- It should have NO partitions (or partitions you don't care about)
- **DOUBLE CHECK** — if you pick the wrong drive you will DESTROY WINDOWS

Write down the drive name (e.g., `/dev/sdb` or `/dev/nvme1n1`).

**FROM HERE ON, I'll use `/dev/sdb` — REPLACE WITH YOUR ACTUAL DRIVE NAME.**

## Phase 4: Partition the Drive (10 minutes)

We'll create 3 partitions:
- **EFI** (512MB) — boot files
- **Swap** (16GB) — virtual memory (match your RAM)
- **Root** (rest) — everything else

```bash
gdisk /dev/sdb
```

### Create EFI partition
```
Command: n          (new partition)
Partition number: 1 (press Enter)
First sector:       (press Enter for default)
Last sector: +512M
Hex code: ef00      (EFI System)
```

### Create swap partition
```
Command: n
Partition number: 2 (press Enter)
First sector:       (press Enter)
Last sector: +16G
Hex code: 8200      (Linux swap)
```

### Create root partition
```
Command: n
Partition number: 3 (press Enter)
First sector:       (press Enter)
Last sector:        (press Enter — uses all remaining space)
Hex code: 8300      (Linux filesystem — press Enter, it's default)
```

### Write and exit
```
Command: w
Do you want to proceed? Y
```

### Format the partitions
```bash
mkfs.fat -F 32 /dev/sdb1        # EFI
mkswap /dev/sdb2                 # Swap
mkfs.ext4 /dev/sdb3              # Root
```

### Mount them
```bash
mount /dev/sdb3 /mnt             # Root
mkdir -p /mnt/boot
mount /dev/sdb1 /mnt/boot        # EFI
swapon /dev/sdb2                  # Swap
```

## Phase 5: Install Arch (15 minutes)

### Install base system
```bash
pacstrap -K /mnt base linux linux-firmware sudo networkmanager vim nano
```
This downloads ~500MB. Wait for it.

### Generate filesystem table
```bash
genfstab -U /mnt >> /mnt/etc/fstab
```

### Enter the new system
```bash
arch-chroot /mnt
```
Your prompt changes — you're now "inside" your new Arch install.

## Phase 6: Configure the System (10 minutes)

### Timezone
```bash
ln -sf /usr/share/zoneinfo/America/New_York /etc/localtime
hwclock --systohc
```
Replace `America/New_York` with your timezone. List them with `ls /usr/share/zoneinfo/`.

### Locale
```bash
echo "en_US.UTF-8 UTF-8" > /etc/locale.gen
locale-gen
echo "LANG=en_US.UTF-8" > /etc/locale.conf
```

### Hostname
```bash
echo "thermal-os" > /etc/hostname
```

### Root password
```bash
passwd
```
Type a password twice.

### Create your user
```bash
useradd -m -G wheel -s /bin/bash builder
passwd builder
```

### Give your user sudo
```bash
EDITOR=nano visudo
```
Find the line `# %wheel ALL=(ALL:ALL) ALL` and remove the `#` at the start.
Save: Ctrl+O, Enter, Ctrl+X.

### Enable networking
```bash
systemctl enable NetworkManager
```

## Phase 7: Boot Loader (10 minutes)

We'll use GRUB — it auto-detects Windows.

```bash
pacman -S grub efibootmgr os-prober
```

### Install GRUB
```bash
grub-install --target=x86_64-efi --efi-directory=/boot --bootloader-id=GRUB
```

### Enable Windows detection
```bash
nano /etc/default/grub
```
Find `#GRUB_DISABLE_OS_PROBER=false` and remove the `#`.
Save and exit.

### Mount Windows EFI (so GRUB finds Windows)
```bash
mkdir -p /mnt/windows-efi
mount /dev/sda1 /mnt/windows-efi    # Your WINDOWS EFI partition - check with lsblk
```

### Generate GRUB config
```bash
grub-mkconfig -o /boot/grub/grub.cfg
```
You should see a line like `Found Windows Boot Manager on /dev/sda1`. If you do, you're golden.

```bash
umount /mnt/windows-efi
```

## Phase 8: Install GPU Drivers

### AMD (Ryzen 5800X has no integrated GPU — skip if you have a discrete GPU)
```bash
pacman -S mesa vulkan-radeon lib32-mesa lib32-vulkan-radeon
```

### NVIDIA
```bash
pacman -S nvidia nvidia-utils lib32-nvidia-utils
```

### Intel
```bash
pacman -S mesa vulkan-intel lib32-mesa
```

Install whichever matches your GPU. If unsure:
```bash
lspci | grep -i vga
```

## Phase 9: Reboot Into Arch (2 minutes)

```bash
exit                    # Leave chroot
umount -R /mnt          # Unmount everything
reboot
```

Remove the USB drive when the screen goes black.

GRUB should appear with:
- **Arch Linux**
- **Windows Boot Manager**

Select Arch Linux. Login as `builder` with your password.

## Phase 10: Deploy Thermal OS (15 minutes)

You're in a bare TTY. Let's get your thermal setup going.

### Connect to internet
```bash
sudo nmtui          # Graphical WiFi setup
# or it just works on ethernet
```

### Install git
```bash
sudo pacman -S git
```

### Clone your dotfiles
```bash
git clone https://github.com/GGPrompts/thermal-os-dotfiles ~/dotfiles
cd ~/dotfiles
```

### Run the bootstrap
```bash
chmod +x bootstrap.sh
./bootstrap.sh
```
This installs all packages and symlinks all configs. Takes 5-10 minutes.

### Clone the desktop suite
```bash
git clone https://github.com/GGPrompts/thermal-desktop ~/projects/thermal-desktop
cd ~/projects/thermal-desktop
cargo build --release
```

### Start Hyprland
```bash
Hyprland
```

You should see the thermal desktop come alive.

## Switching Between Windows and Arch

- **Restart** → GRUB menu appears → pick your OS
- Or mash your BIOS boot key and pick from there
- Windows is untouched on C: — nothing changed

## If Something Goes Wrong

### Can't boot into Windows anymore?
Boot from USB again, then:
```bash
mount /dev/sdb1 /mnt
grub-install --target=x86_64-efi --efi-directory=/mnt --bootloader-id=GRUB
mount /dev/sda1 /mnt2    # Windows EFI
grub-mkconfig -o /mnt/grub/grub.cfg
```

### Can't boot into Arch?
Boot from USB:
```bash
mount /dev/sdb3 /mnt
mount /dev/sdb1 /mnt/boot
arch-chroot /mnt
# Now you're back in your Arch install, fix whatever broke
```

### Want to start over?
Boot Windows, open Disk Management, delete partitions on D:, done.
Arch is gone, Windows is fine.

### WiFi not working after install?
```bash
sudo systemctl start NetworkManager
sudo nmtui
```

## Quick Reference

| What | Command |
|------|---------|
| Update system | `sudo pacman -Syu` |
| Install package | `sudo pacman -S package-name` |
| Search packages | `pacman -Ss search-term` |
| Start Hyprland | `Hyprland` |
| Start thermal-conductor | `cargo run -p thermal-conductor` |
| Switch to Windows | Reboot, pick Windows in GRUB |
| Connect WiFi | `sudo nmtui` |
| Edit configs | `cd ~/dotfiles && nvim` |
