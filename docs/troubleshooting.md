# Troubleshooting

Common issues and how to resolve them.

## Connection Issues

### "Connection refused"

**Symptom:**
```
Error: Connection refused (os error 111)
```

**Causes:**
- SPICE server not running
- Wrong host or port
- Firewall blocking connection

**Solutions:**
1. Verify the SPICE server is running:
   ```bash
   # Check if port is listening
   nc -zv <host> <port>
   ```
2. Check firewall rules on server
3. Verify the .vv file has correct host/port

### "Server requires TLS connection"

**Symptom:**
```
Error: Link error: NeedSecured
```

**Cause:** Server only accepts TLS connections, but you connected to the
insecure port.

**Solution:** Use `tls-port` in your .vv file, or specify both ports with
`--direct`:
```bash
ryll --direct 192.168.1.100:5900:5901
```

### "Authentication failed"

**Symptom:**
```
Error: Authentication failed: PermissionDenied
```

**Causes:**
- Wrong password
- No password provided when required
- Password encoding issue

**Solutions:**
1. Verify password in .vv file is correct
2. Check if server requires a password:
   ```bash
   # In QEMU, check -spice options
   ```
3. Try quoting the password if it contains special characters

### TLS Certificate Errors

**Symptom:**
```
Error: invalid peer certificate: UnknownIssuer
```

**Cause:** Server's TLS certificate isn't trusted.

**Solutions:**
1. Add the CA certificate to your .vv file:
   ```ini
   ca=/path/to/ca.pem
   ```
2. For testing, the server may need to use a certificate from a known CA

## Display Issues

### "Waiting for display..." stays forever

**Symptom:** GUI shows "Waiting for display..." but never shows content.

**Causes:**
- Server isn't sending display data
- Display channel didn't connect properly
- Decompression errors (check verbose output)

**Solutions:**
1. Enable verbose logging:
   ```bash
   ryll --file test.vv -v
   ```
2. Check that the VM has a display configured
3. Look for decompression errors in the log

### Black or corrupted display

**Symptom:** Window appears but content is black or garbled.

**Causes:**
- Image decompression failing
- Surface size mismatch
- Pixel format issues

**Solutions:**
1. Enable verbose logging to see decompression errors
2. Try a different VM or display configuration
3. Report the issue with verbose log output

## Input Issues

### Keyboard input not working

**Symptom:** Key presses in the window don't reach the VM.

**Causes:**
- Inputs channel didn't connect
- Focus not on the display window
- Scancode mapping issue for your keyboard layout

**Solutions:**
1. Click on the display area to ensure focus
2. Check verbose logs for inputs channel connection
3. Some special keys may not be mapped yet

### Mouse not working

**Symptom:** Mouse movements or clicks don't register.

**Causes:**
- Need to click in the display area first
- Server not in correct mouse mode

**Solutions:**
1. Click inside the display area
2. Check if server supports client mouse mode

## Performance Issues

### High CPU usage

**Symptom:** ryll uses excessive CPU even when display is static.

**Causes:**
- Continuous repaint requests
- Decompression overhead

**Solutions:**
1. This may be normal for egui's immediate mode rendering
2. In headless mode, CPU usage should be much lower
3. Check if server is sending unnecessary updates

### High latency

**Symptom:** Noticeable delay between input and display response.

**Causes:**
- Network latency
- Server processing time
- Proxy overhead (if using kerbside)

**Solutions:**
1. Use `--cadence --latency-file latency.csv` to measure
2. Compare with direct connection (no proxy)
3. Check network conditions

## Build Issues

### Missing graphics libraries

**Symptom:**
```
error: failed to run custom build command for `eframe`
```

**Cause:** Missing X11/OpenGL development libraries.

**Solution:** Install required dependencies:
```bash
apt-get install -y \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb1-dev \
    libx11-dev libxkbcommon-dev libgl1-mesa-dev libegl1-mesa-dev \
    libwayland-dev libssl-dev pkg-config
```

Or use the devcontainer:
```bash
make build
```

### Binary won't run on another machine

**Symptom:**
```
error while loading shared libraries: libxcb.so.1
```

**Cause:** Target machine is missing required libraries.

**Solution:** See [portability.md](portability.md) for details on binary
compatibility.

## Debugging Tips

### Enable verbose logging

```bash
ryll --file test.vv -v 2>&1 | tee debug.log
```

### Check what channels connected

Look for lines like:
```
INFO Connected to main channel successfully
INFO Connected to display channel successfully
INFO Connected to inputs channel successfully
INFO Connected to cursor channel successfully
```

### Monitor network traffic

```bash
# See SPICE traffic (unencrypted only)
tcpdump -i any port 5900 -w spice.pcap
```

### Test with headless mode first

Headless mode eliminates GUI-related issues:
```bash
ryll --file test.vv --headless -v
```

If headless works but GUI doesn't, the issue is in the rendering layer.

## Getting Help

If you can't resolve an issue:

1. Collect verbose logs: `ryll --file test.vv -v 2>&1 | tee debug.log`
2. Note the exact error message
3. Note your OS, Rust version, and how you built ryll
4. Open an issue on the GitHub repository
