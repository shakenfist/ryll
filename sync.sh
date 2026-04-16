#rsync -av openclaw@dev:ryll --exclude /ryll/target ../
cargo build --release
rm /tmp/ryll.log
./target/release/ryll --monitors=1 --file ~/Downloads/conn.vv -v > /dev/null
