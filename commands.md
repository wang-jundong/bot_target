# Run bot_target (systemd)

Run the release binary as a systemd service so it starts on boot and restarts on failure.

Paths assume the project lives at `/home/ubuntu/bot_target` on the server. Change them if yours differs.

## 0. Build release (on the server)

```bash
cd /home/ubuntu/bot_target
cargo build --release
```

## 1. Create the service file

```bash
sudo nano /etc/systemd/system/bot-target.service
```

Paste the following:

```ini
[Unit]
Description=bot_target strategies v011/v022/v031
After=network.target

[Service]
Type=simple
WorkingDirectory=/home/ubuntu/bot_target
ExecStart=/home/ubuntu/bot_target/bot_target
Restart=always
RestartSec=5
User=ubuntu

[Install]
WantedBy=multi-user.target
```

## 2. Enable and start

```bash
sudo systemctl daemon-reload
sudo systemctl enable bot-target
sudo systemctl start bot-target
```

## 3. Everyday commands

```bash
# status
sudo systemctl status bot-target

# logs (follow)
sudo journalctl -u bot-target -f

# stop / start / restart
sudo systemctl stop bot-target
sudo systemctl start bot-target
sudo systemctl restart bot-target
```

## 4. After code or bindings change

```bash
cd /home/ubuntu/bot_target
cargo build --release
sudo systemctl restart bot-target
```
