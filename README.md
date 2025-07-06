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
   - Check if the repository exists on Hugging Face
   - Calculate total size and verify it's under the 60GB limit
   - Use Git LFS-aware cloning (proper repository cloning with `git clone`)
   - Identify and download Git LFS files separately via HTTP
   - Generate a torrent file from the complete repository
   - Provide a magnet link for sharing

## Technical Improvements

### Git LFS-Aware Repository Cloning

Unlike simple HTTP file downloading, muggingface now uses a proper Git-based approach:

- **Git Clone with LFS Skip**: Uses `GIT_LFS_SKIP_SMUDGE=1 git clone` to get the repository structure first
- **LFS File Detection**: Identifies Git LFS files using `git lfs ls-files` 
- **Selective LFS Download**: Downloads only the actual LFS file content via HTTP resolve URLs
- **Repository Integrity**: Maintains proper Git repository structure and metadata
- **Efficient Caching**: Avoids re-downloading files that are already present and complete

This approach is similar to how the official `huggingface_hub` Python SDK works and ensures better compatibility and reliability.

### Authentication Support

Muggingface supports Hugging Face authentication tokens for:

- **Private Repositories**: Access private models, datasets, and spaces with your HF token
- **Gated Repositories**: Download gated content you have access to
- **Higher Rate Limits**: Avoid API throttling with authenticated requests

To use authentication, add your Hugging Face token to the `Secrets.toml` file:
```toml
HF_TOKEN = "hf_your_token_here"
```

Get your token from [https://huggingface.co/settings/tokens](https://huggingface.co/settings/tokens).

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
# Optional: Add HF token for private repository access
HF_TOKEN = "hf_your_token_here"
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
