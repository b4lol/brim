# Brim — Yapılacaklar ve Yol Haritası (TODO & Roadmap)

Bu belge, Brim projesinin mimari değerlendirmesi ve analizine dayalı olarak planlanan iyileştirme, yeni özellik ve yol haritası maddelerini listeler.

---

## 1. Platform & Yeni Backend Destekleri

- [ ] **Arch Linux Desteği (`pacman` / AUR)**
  - [ ] `pacman` CLI sarmalayıcısı ile paket arama, listeleme, kurma ve güncelleme desteği.
  - [ ] AUR (Arch User Repository) yardımcıları (`paru` / `yay`) veya doğrudan AUR RPC API entegrasyonu.
- [ ] **Snap / Snapcraft Entegrasyonu**
  - [ ] `snapd` REST API veya `snap` CLI aracılığıyla Snap paketleri desteği.
  - [ ] Flatpak ile paralel çalışabilecek şekilde yapılandırma yönetimi.
- [ ] **Deklaratif Paket Yöneticileri (Deneysel)**
  - [ ] Nix (`nix-env` / Flakes) veya GNU Guix için temel sorgu desteği araştırması.

---

## 2. GUI (GTK4 / Libadwaita) Geliştirmeleri

- [ ] **Depolama ve Boyut Analiz Paneli**
  - [ ] Kurulu paketlerin ve önbelleklerin disk kullanımını gösteren grafiksel analiz ekranı.
  - [ ] Büyük boyutlu ve uzun süredir kullanılmayan paketleri listeleme.
- [ ] **Bağımlılık ve Ters Bağımlılık Ağacı**
  - [ ] Paket detay diyalogunda bağımlılıklar (`requires`) ve bu pakete bağımlı diğer paketler (`required-by`) görünümü.
- [ ] **Zengin Medya ve Ekran Görüntüleri**
  - [ ] Flathub Appstream verilerini kullanarak uygulama ekran görüntüleri ve zengin açıklamaları galeride gösterme.
- [ ] **Arka Plan Bildirimleri ve Sistem Tepsisi**
  - [ ] Sistem güncellemeleri çıktığında kullanıcıyı uyaran masaüstü bildirimleri (`libnotify` / FreeDesktop bildirimleri).

---

## 3. Web Arayüzü & REST API İyileştirmeleri

- [ ] **Gerçek Zamanlı İlerleme ve Log Akışı (Streaming)**
  - [ ] Uzun süren işlemler (ör. toplu sistem güncellemesi, büyük paket indirmeleri) için Server-Sent Events (SSE) veya WebSocket desteği.
  - [ ] Web arayüzünde terminal benzeri canlı çıktı paneli.
- [ ] **Gelişmiş Tema ve Arayüz Seçenekleri**
  - [ ] Sistem tercihlerine duyarlı ve manuel değiştirilebilir Karanlık / Aydınlık (Dark/Light) tema desteği.
  - [ ] Mobil uyumlu (responsive) dokunmatik arayüz iyileştirmeleri.
- [ ] **İşlem Geçmişi ve Raporlama**
  - [ ] `/api/history` uç noktası ile geçmişte yapılan kurulum, kaldırma ve güncelleme işlemlerinin JSON dökümü.

---

## 4. CLI (Terminal) İyileştirmeleri

- [ ] **Yetim Paket Temizleme (`brim autoremove` / `brim clean`)**
  - [ ] Artık ihtiyaç duyulmayan kullanılmayan bağımlılıkları ve eski paket önbelleklerini temizleme komutu.
- [ ] **İşlem Geçmişi ve Geri Alma (`brim history` / `brim rollback`)**
  - [ ] DNF5 ve APT işlem geçmişini listeleyebilme ve desteklenen backend'lerde önceki sürüme geri dönebilme.
- [ ] **Zenginleştirilmiş İlerleme Göstergeleri**
  - [ ] İndirme ve kurulum aşamalarında daha ayrıntılı hız, kalan süre ve transfer boyutu göstergeleri.

---

## 5. Paketleme, Dağıtım ve DevOps

- [ ] **Dağıtım Paketleri Oluşturma**
  - [ ] Fedora / RHEL için `.rpm` paketi spec dosyası ve otomatik paketleme tanımları.
  - [ ] Debian / Ubuntu için `.deb` (`dpkg-buildpackage`) tanımları.
- [ ] **Taşınabilir Formatlar**
  - [ ] Bağımsız AppImage ve Flatpak (`org.b4lol.Brim`) manifestoları.
- [ ] **CI/CD ve Otomatik Dağıtım**
  - [ ] GitHub Actions üzerinde etiketlenen (tagged) sürümler için otomatik çoklu mimari (x86_64, aarch64) ikili derleme ve GitHub Releases yükleme.
  - [ ] Güvenlik için ikili dosyaların `cosign` veya GPG ile otomatik imzalanması.
