# Brightness Control

A modern, lightweight Windows desktop application built with **Tauri v2**, **Rust**, and **TypeScript** for seamless hardware brightness adjustment across both internal laptop displays and external monitors.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%2F%2011-0078D6.svg)](https://microsoft.com/windows)
[![Tauri](https://img.shields.io/badge/Tauri-v2-FFC131.svg)](https://tauri.app)

---

## Key Features

- **Multi-Monitor Control**:
  - **Internal / Laptop Displays**: Native brightness adjustment via **WMI** (`WmiMonitorBrightness` / `WmiMonitorBrightnessMethods`).
  - **External Monitors**: Hardware-level control via **DDC/CI** standard (`dxva2.dll` / VCP code `0x10`).
- **Windows 11 Fluent UI**:
  - Automatically adapts to system **Light & Dark modes**.
  - Smooth slider controls with quick preset buttons (**25%**, **50%**, **75%**, **100%**) and **±5%** stepper buttons.
  - **Master Control Slider**: Synchronizes all active displays simultaneously when two or more controllable displays are detected.
- **System Tray Flyout Integration**:
  - Click the system tray icon to reveal a sleek flyout slider panel (styled like Windows 11's native volume flyout).
  - Real-time bidirectional state synchronization between the main window and tray flyout.
- **Safety & Productivity**:
  - **Black-Screen Protection**: Enforces a minimum brightness floor (**10%**) to prevent accidental complete blackouts.
  - **Start with Windows**: Optional background launch on Windows boot directly to the system tray.
  - **Keyboard Shortcuts**: `F5` to rescan displays, `Esc` to minimize to system tray.

---

## Troubleshooting & Tips

- **External Monitor Not Responding to Brightness Changes?**
  - **Enable DDC/CI in OSD**: Open your monitor's On-Screen Display (OSD) menu using its physical buttons, locate the **DDC/CI** setting, and ensure it is switched to **On**.
  - **Display Cable Connection**: Connect your monitor directly via HDMI or DisplayPort. Some USB-C hubs, docking stations, and KVM switches filter out DDC/CI I2C signals.
- **Desktop Computers**:
  - WMI controls only exist on laptops with integrated panels. External monitors attached to desktop systems communicate exclusively via DDC/CI.

---

## Contributing

Contributions, issues, and feature requests are welcome! Feel free to check the [issues page](https://github.com/k4han/brightness-control/issues).

1. Fork the Project
2. Create your Feature Branch (`git checkout -b feature/AmazingFeature`)
3. Commit your Changes (`git commit -m 'Add some AmazingFeature'`)
4. Push to the Branch (`git push origin feature/AmazingFeature`)
5. Open a Pull Request

---

## License

Distributed under the MIT License. See [`LICENSE`](LICENSE) for more information.

---

## ☕ Support

If you find this plugin helpful and want to support its development, consider buying me a coffee!

[![Buy Me A Coffee](https://img.shields.io/badge/Buy_Me_A_Coffee-FFDD00?style=for-the-badge&logo=buy-me-a-coffee&logoColor=black)](https://buymeacoffee.com/kh4n)