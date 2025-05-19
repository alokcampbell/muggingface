# Muggingface

A web service that helps you download Huggingface model repositories as torrents. This allows for faster, more reliable downloads of large model files through peer-to-peer sharing.

https://muggingface.co

## Features

- Convert any Hugging Face repository into a torrent
- Real-time download progress tracking
- Automatic magnet link generation
- Persistent storage of torrent files
- Support for repositories up to 60GB in size
- Built with Rust and Actix-web for high performance

## How It Works

1. Visit `https://muggingface.com/{username}/{repository}`
2. The service will:
   - Check if the repository exists
   - Calculate total size
   - Download files if not already present
   - Generate a torrent file
   - Provide a magnet link for sharing

## Technical Details

- Built with Rust and Actix-web
- Uses PostgreSQL for persistent storage
- Implements the BitTorrent protocol for file sharing
- Supports multiple trackers for better availability
- Includes progress tracking and status updates

## Development

### Prerequisites

- Rust (latest stable)
- PostgreSQL
- Cargo

### Setup

1. Clone the repository:
```bash
git clone https://github.com/alokcampbell/muggingface.git
cd muggingface
```

2. Create a `Secrets.toml` file with your database configuration:
```toml
DATABASE_URL = "postgres://username:password@localhost:5432/muggingface"
```

3. Run database migrations:
```bash
sqlx database create
sqlx migrate run
```

4. Start the development server:
```bash
cargo run
```

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
