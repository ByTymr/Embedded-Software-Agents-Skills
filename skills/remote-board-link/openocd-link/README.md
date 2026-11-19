# openocd-link

Başka ağdaki bir bilgisayara takılı geliştirme kartına (ST-Link + OpenOCD) internet üzerinden,
sunucusuz ve uçtan uca şifreli erişim.

Anahtarı bilen bağlanır. Buluşma BitTorrent DHT ve herkese açık STUN ile, bağlantı QUIC/UDP
delme ile kurulur. İstemci ve servis doğrudan birbirine bağlanır. Simetrik NAT veya
kararsız STUN ölçümünde çoklu UDP uçlarıyla ek bağlantı yolları denenir. Her iki uç
simetrik NAT/CGNAT arkasındaysa doğrudan bağlantı kurulamayabilir.

## Uzak cihazda (bir kez)

```
sudo ./packaging/linux/install-link.sh ./openocd-link
#   Bağlantı anahtarı: <connection-key>   (bir kez gösterilir)
sudo nano /etc/openocd-link/config.toml        # [openocd].args değerini kartına göre yaz
sudo systemctl restart openocd-link
```

Servis yalnız Linux/systemd üzerinde çalışır. `exec` ve `shell` varsayılan olarak kapalıdır;
açılırsa anahtarı bilen herkes servis kullanıcısının yetkisiyle komut çalıştırabilir.

## Kendi bilgisayarında

```
openocd-link import-key --out ~/.config/openocd-link/lab.key      # anahtarı yapıştır
openocd-link --session lab connect --key-file ~/.config/openocd-link/lab.key
# Arama veya bağlantı kesilirse yeniden dener. Tek deneme için --once kullanın.
# başka terminalde:
openocd-link --session lab openocd start
eat --profile projects/kart/board.json identity
eat --profile projects/kart/board.json flash --execute            # openocd.remote_files: true
arm-none-eabi-gdb build/firmware.axf -ex "target extended-remote 127.0.0.1:3333"
```

Komutlar: `keygen`, `import-key`, `serve`, `connect`, `status`, `disconnect`, `info`, `ls`,
`put`, `get`, `rm`, `openocd start|stop|status|log`, `exec -- KOMUT...`, `shell`.
`--json` makine çıktısı verir.

UDP portu yönlendirilmiş istemci için `connect --port 47124` ve servis yapılandırmasında
`connect_to = ["<istemci-ip>:47124"]` kullanın. Sabit adres yolunda istemci ve
servisi birlikte güncelleyin. Ters yönde açılan akış artık uygulama kimlik
doğrulamasından önce bir bağlantı işareti ve yanıtı kullanır.

Yerel tünel portları `127.0.0.1` üzerindedir; aynı makinedeki diğer kullanıcılar bağlanabilir.
Kontrol soketi jetonla ve 0600 izinli durum dosyasıyla korunur.

## Geliştirme

```
cargo build --workspace
cargo test --workspace
```

`crates/openocd-link-core`: ağ çekirdeği (Ulac/neyber'den alınan NAT delme, DHT, Noise+SPAKE2,
tünel). `crates/openocd-link`: CLI ve servis. Protokol sabitleri (`OL1-`, `OCDLNK01`,
türetme bağlamları) neyber ile uyumlu değildir.
