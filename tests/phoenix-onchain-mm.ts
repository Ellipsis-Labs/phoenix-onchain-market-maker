import * as anchor from "@coral-xyz/anchor";
import { AnchorProvider, BN, Program } from "@coral-xyz/anchor";
import { PhoenixOnchainMm } from "../target/types/phoenix_onchain_mm";
import {
  Connection,
  Keypair,
  PublicKey,
  sendAndConfirmTransaction,
  SystemProgram,
  Transaction,
  TransactionInstruction,
} from "@solana/web3.js";

import {
  createAssociatedTokenAccountInstruction,
  createMintToInstruction,
  getAssociatedTokenAddress,
  NATIVE_MINT,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { assert } from "chai";

import { TokenConfig } from "@ellipsis-labs/phoenix-sdk";
import * as Phoenix from "@ellipsis-labs/phoenix-sdk";

// DO NOT USE THIS PRIVATE KEY IN PRODUCTION
// This key is the market authority as well as the market maker
const god = Keypair.fromSeed(
  new Uint8Array([
    65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65,
    65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65, 65,
  ])
);

// Hardcoded market address of SOL/USDC Phoenix market
// This market is loaded at genesis
const solMarketAddress = new PublicKey(
  "HhHRvLFvZid6FD7C96H93F2MkASjYfYAx8Y2P8KMAr6b"
);

const usdcMint = new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

const tokenConfig: TokenConfig[] = [
  {
    name: "USD Coin",
    symbol: "USDC",
    mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    logoUri:
      "https://raw.githubusercontent.com/solana-labs/token-list/main/assets/mainnet/EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v/logo.png",
  },
  {
    name: "Wrapped SOL",
    symbol: "SOL",
    mint: "So11111111111111111111111111111111111111112",
    logoUri:
      "https://raw.githubusercontent.com/solana-labs/token-list/main/assets/mainnet/So11111111111111111111111111111111111111112/logo.png",
  },
];

const createTokenAccountInstructions = async (
  provider: AnchorProvider,
  tokenMintAddress: PublicKey,
  owner?: PublicKey
): Promise<[PublicKey, TransactionInstruction]> => {
  owner = owner || provider.wallet.publicKey;

  const userTokenAccount = await getAssociatedTokenAddress(
    tokenMintAddress,
    owner
  );

  const createAta = createAssociatedTokenAccountInstruction(
    provider.wallet.publicKey,
    userTokenAccount,
    owner,
    tokenMintAddress
  );

  return [userTokenAccount, createAta];
};

const createWSOLAccount = async (
  provider: AnchorProvider,
  mintAmount?: BN,
  owner?: PublicKey
): Promise<PublicKey> => {
  const tx = new Transaction();
  const [userWSOLAccount, createAta] = await createTokenAccountInstructions(
    provider,
    NATIVE_MINT,
    owner
  );
  if (mintAmount && mintAmount > new BN(0)) {
    const transferIx = SystemProgram.transfer({
      fromPubkey: provider.wallet.publicKey,
      toPubkey: userWSOLAccount,
      lamports: mintAmount.toNumber(),
    });
    tx.add(transferIx);
  }
  tx.add(createAta);
  await sendAndConfirmTransaction(
    provider.connection,
    tx,
    // @ts-ignore
    [provider.wallet.payer],
    {
      skipPreflight: true,
      commitment: "confirmed",
      preflightCommitment: "confirmed",
    }
  );
  return userWSOLAccount;
};

const createTokenAccountAndMintTokens = async (
  provider: AnchorProvider,
  tokenMintAddress: PublicKey,
  mintAmount: BN,
  mintAuthority: Keypair,
  owner?: PublicKey
): Promise<PublicKey> => {
  const tx = new Transaction();

  const [userTokenAccount, createAta] = await createTokenAccountInstructions(
    provider,
    tokenMintAddress,
    owner
  );

  tx.add(createAta);

  const mintToUserAccountTx = await createMintToInstruction(
    tokenMintAddress,
    userTokenAccount,
    mintAuthority.publicKey,
    mintAmount.toNumber()
  );
  tx.add(mintToUserAccountTx);

  await sendAndConfirmTransaction(
    provider.connection,
    tx,
    // @ts-ignore
    [provider.wallet.payer, mintAuthority],
    {
      skipPreflight: false,
    }
  );

  return userTokenAccount;
};

const createPhoenixClient = async (
  connection: Connection
): Promise<Phoenix.Client> => {
  const client = await Phoenix.Client.createWithoutConfig(connection, []);
  client.tokenConfig = tokenConfig;
  await client.addMarket(solMarketAddress.toBase58());
  return client;
};

describe("phoenix-onchain-mm", () => {
  // Configure the client to use the local cluster.

  const provider = anchor.AnchorProvider.local(undefined, {
    commitment: "confirmed",
    skipPreflight: false,
    preflightCommitment: "confirmed",
  });
  const connection = provider.connection;
  anchor.setProvider(provider);

  const program = anchor.workspace
    .PhoenixOnchainMm as Program<PhoenixOnchainMm>;

  let makerUsdcTokenAccount: PublicKey;

  let makerWrappedSolTokenAccount: PublicKey;

  let phoenixClient: Phoenix.Client;

  before(async () => {
    phoenixClient = await createPhoenixClient(connection);
    const phoenixMarket = phoenixClient.markets.get(
      solMarketAddress.toBase58()
    );
    assert(phoenixMarket.data.header.authority.equals(god.publicKey));
    assert(phoenixMarket.data.traders.has(god.publicKey.toBase58()));

    // Top-up god key's SOL balance
    await sendAndConfirmTransaction(
      connection,
      new Transaction().add(
        SystemProgram.transfer({
          fromPubkey: provider.wallet.publicKey,
          toPubkey: god.publicKey,
          lamports: 10000000000000,
        })
      ),
      // @ts-ignore
      [provider.wallet.payer],
      { commitment: "confirmed" }
    );

    makerUsdcTokenAccount = await createTokenAccountAndMintTokens(
      provider,
      usdcMint,
      new BN(100_000 * 1e6),
      god,
      god.publicKey
    );

    makerWrappedSolTokenAccount = await createWSOLAccount(
      provider,
      new BN(5_000 * 1e9),
      god.publicKey
    );

    console.log("Minted tokens");
    console.log("Maker USDC token account", makerUsdcTokenAccount.toString());
    console.log(
      "Maker WSOL token account",
      makerWrappedSolTokenAccount.toString()
    );
  });
  it("Is initialized!", async () => {
    const params = {
      quoteEdgeInBps: new BN(2),
      quoteSizeInQuoteAtoms: new BN(500 * 1e6),
      postOnly: false,
      priceImprovementBehavior: {
        ignore: {},
      },
    };

    const tx = await program.methods
      .initialize(params)
      .accounts({
        user: god.publicKey,
        market: solMarketAddress,
        systemProgram: SystemProgram.programId,
      })
      .signers([god])
      .rpc();

    console.log("Initialize:", tx);
    const phoenixMarket = phoenixClient.markets.get(
      solMarketAddress.toBase58()
    );
    for (let i = 0; i < 20; i++) {
      const price = await fetch(
        "https://api.coinbase.com/v2/prices/SOL-USD/spot"
      )
        .then((response) => response.json())
        .then((data) => {
          return data.data.amount;
        })
        .catch((error) => console.error(error));
      const tx = await program.methods
        .updateQuotes({
          fairPriceInQuoteAtomsPerRawBaseUnit: new BN(Math.floor(price * 1e6)),
          strategyParams: params,
        })
        .accounts({
          user: god.publicKey,
          market: solMarketAddress,
          phoenixProgram: Phoenix.PROGRAM_ID,
          logAuthority: Phoenix.getLogAuthority(),
          seat: phoenixMarket.getSeatAddress(god.publicKey),
          quoteAccount: makerUsdcTokenAccount,
          baseAccount: makerWrappedSolTokenAccount,
          quoteVault: phoenixMarket.data.header.quoteParams.vaultKey,
          baseVault: phoenixMarket.data.header.baseParams.vaultKey,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([god])
        .rpc({ skipPreflight: true });
      console.log("Update, price =", price, ":", tx);
      await new Promise((r) => setTimeout(r, 1000));
    }
  });
});
