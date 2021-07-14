import * as anchor from '@project-serum/anchor'
import { Program } from '@project-serum/anchor'
import { Token, TOKEN_PROGRAM_ID } from '@solana/spl-token'
import { Account, PublicKey } from '@solana/web3.js'
import { assert } from 'chai'
import { BN, Exchange, Network } from '@synthetify/sdk'

import {
  createAssetsList,
  EXCHANGE_ADMIN,
  tou64,
  SYNTHETIFY_ECHANGE_SEED,
  createAccountWithCollateralAndMaxMintUsd,
  assertThrowsAsync,
  mulByPercentage,
  createCollateralToken,
  createToken,
  waitForBeggingOfASlot
} from './utils'
import { createPriceFeed } from './oracleUtils'

const ASSET_LIMIT = 30

describe('max collaterals', () => {
  const provider = anchor.Provider.local()
  const connection = provider.connection
  const exchangeProgram = anchor.workspace.Exchange as Program
  let exchange: Exchange

  const oracleProgram = anchor.workspace.Pyth as Program

  // @ts-expect-error
  const wallet = provider.wallet.payer as Account
  // let collateralToken: Token
  let usdToken: Token
  let btcSynthetic: Token
  let collateralTokenFeed: PublicKey
  let assetsList: PublicKey
  let exchangeAuthority: PublicKey
  // let snyReserve: PublicKey
  let snyLiquidationFund: PublicKey
  let stakingFundAccount: PublicKey
  let nonce: number
  let tokens: Token[] = []
  let reserves: PublicKey[] = []
  let healthFactor: BN

  before(async () => {
    const [_mintAuthority, _nonce] = await anchor.web3.PublicKey.findProgramAddress(
      [SYNTHETIFY_ECHANGE_SEED],
      exchangeProgram.programId
    )
    nonce = _nonce
    exchangeAuthority = _mintAuthority
    collateralTokenFeed = await createPriceFeed({
      oracleProgram,
      initPrice: 2
      // expo: -6
    })

    const collateralToken = await createToken({
      connection,
      payer: wallet,
      mintAuthority: wallet.publicKey
    })
    const snyReserve = await collateralToken.createAccount(exchangeAuthority)
    snyLiquidationFund = await collateralToken.createAccount(exchangeAuthority)
    stakingFundAccount = await collateralToken.createAccount(exchangeAuthority)

    tokens.push(collateralToken)
    reserves.push(snyReserve)
    // @ts-expect-error
    exchange = new Exchange(
      connection,
      Network.LOCAL,
      provider.wallet,
      exchangeAuthority,
      exchangeProgram.programId
    )

    const data = await createAssetsList({
      exchangeAuthority,
      collateralToken,
      collateralTokenFeed,
      connection,
      wallet,
      exchange,
      snyReserve,
      snyLiquidationFund
    })
    assetsList = data.assetsList
    usdToken = data.usdToken
    tokens.push(usdToken)
    reserves.push(await usdToken.createAccount(exchangeAuthority))

    await exchange.init({
      admin: EXCHANGE_ADMIN.publicKey,
      assetsList,
      nonce,
      amountPerRound: new BN(100),
      stakingRoundLength: 300,
      stakingFundAccount: stakingFundAccount
    })
    exchange = await Exchange.build(
      connection,
      Network.LOCAL,
      provider.wallet,
      exchangeAuthority,
      exchangeProgram.programId
    )

    healthFactor = new BN((await exchange.getState()).healthFactor)
    const createCollateralProps = {
      exchange,
      exchangeAuthority,
      oracleProgram,
      connection,
      wallet
    }

    // creating BTC
    const { token: btcToken, synthetic, reserve: btcReserve } = await createCollateralToken({
      decimals: 10,
      price: 50000,
      limit: new BN(1e12),
      collateralRatio: 10,
      ...createCollateralProps
    })
    btcSynthetic = synthetic
    tokens.push(btcToken)
    reserves.push(btcReserve)

    const assetsListBefore = await exchange.getAssetsList(assetsList)
    assert.ok((await assetsListBefore).assets.length)

    // creating tokens asynchronously so it doesn't take 2 minutes (downside is random order)
    const createdTokens = await Promise.all(
      [...Array(ASSET_LIMIT - 3).keys()].map(() =>
        createCollateralToken({
          decimals: 6,
          price: 2,
          limit: new BN(1e12),
          ...createCollateralProps
        })
      )
    )

    const assetsListAfter = await exchange.getAssetsList(assetsList)
    assert.ok(assetsListAfter.head == ASSET_LIMIT)

    // sorting to match order
    const sortedTokens = assetsListAfter.assets
      .slice(3)
      .map(({ feedAddress }) => createdTokens.find((i) => i.feed.equals(feedAddress)))

    assert.ok(sortedTokens.every((token) => token != undefined))

    tokens = tokens.concat(sortedTokens.map((i) => i.token))
    reserves = reserves.concat(sortedTokens.map((i) => i.reserve))
    assert.ok(sortedTokens.length == ASSET_LIMIT - 3)
    assert.ok(tokens.length == ASSET_LIMIT)
    assert.ok(reserves.length == ASSET_LIMIT)
  })
  it('Initialize', async () => {
    const state = await exchange.getState()
    // Check initialized addreses
    assert.ok(state.admin.equals(EXCHANGE_ADMIN.publicKey))
    assert.ok(state.halted === false)
    assert.ok(state.assetsList.equals(assetsList))
    // Check initialized parameters
    assert.ok(state.nonce === nonce)
    assert.ok(state.maxDelay === 0)
    assert.ok(state.fee === 300)
    assert.ok(state.debtShares.eq(new BN(0)))
    assert.ok(state.accountVersion === 0)
  })
  it('creating assets over limit', async () => {
    await assertThrowsAsync(
      createCollateralToken({
        exchange,
        exchangeAuthority,
        oracleProgram,
        connection,
        wallet,
        price: 1,
        decimals: 6
      })
    )
  })
  it('deposit', async () => {
    const accountOwner = new Account()
    const exchangeAccount = await exchange.createExchangeAccount(accountOwner.publicKey)

    await Promise.all(
      tokens.slice(2, 5).map(async (collateralToken, index) => {
        const tokenIndeks = index + 2
        const reserveAccount = reserves[index + 2]

        const userCollateralTokenAccount = await collateralToken.createAccount(
          accountOwner.publicKey
        )
        const amount = new anchor.BN(10 * 1e6)
        await collateralToken.mintTo(userCollateralTokenAccount, wallet, [], tou64(amount))

        await exchange.deposit({
          amount,
          exchangeAccount,
          owner: accountOwner.publicKey,
          userCollateralAccount: userCollateralTokenAccount,
          reserveAccount: reserves[tokenIndeks],
          collateralToken,
          exchangeAuthority,
          signers: [wallet, accountOwner]
        })

        // Check saldos
        const exchangeCollateralTokenAccountInfoAfter = await collateralToken.getAccountInfo(
          reserveAccount
        )
        assert.ok(exchangeCollateralTokenAccountInfoAfter.amount.eq(amount))

        const userExchangeAccountAfter = await exchange.getExchangeAccount(exchangeAccount)
        assert.ok(userExchangeAccountAfter.collaterals[index].amount.eq(amount))
        const assetListData = await exchange.getAssetsList(assetsList)
        assert.ok(assetListData.assets[tokenIndeks].collateral.reserveBalance.eq(amount))
      })
    )
  })
  it('mint', async () => {
    const accountOwner = new Account()
    const exchangeAccount = await exchange.createExchangeAccount(accountOwner.publicKey)

    // Deposit collaterals
    // btc collateral: 50000 * 0,001 * 0,1 = 5
    // other collaterals: 2 * 2 * 10 * 0,5 = 20
    await Promise.all(
      tokens.slice(2, 5).map(async (collateralToken, index) => {
        const tokenIndeks = index + 2

        const userCollateralTokenAccount = await collateralToken.createAccount(
          accountOwner.publicKey
        )
        const amount = new anchor.BN(10 * 1e6)
        await collateralToken.mintTo(userCollateralTokenAccount, wallet, [], tou64(amount))

        await exchange.deposit({
          amount,
          exchangeAccount,
          owner: accountOwner.publicKey,
          userCollateralAccount: userCollateralTokenAccount,
          reserveAccount: reserves[tokenIndeks],
          collateralToken: tokens[tokenIndeks],
          exchangeAuthority,
          signers: [wallet, accountOwner]
        })
      })
    )

    assert.ok((await exchange.getExchangeAccount(exchangeAccount)).debtShares.eq(new BN(0)))

    const usdTokenAccount = await usdToken.createAccount(accountOwner.publicKey)
    const mintAmount = mulByPercentage(new BN(25 * 1e6), healthFactor)

    // Mint xUSD
    await exchange.mint({
      amount: mintAmount,
      exchangeAccount,
      owner: accountOwner.publicKey,
      to: usdTokenAccount,
      signers: [accountOwner]
    })

    // Check saldo and debt shares
    const exchangeAccountAfter = await exchange.getExchangeAccount(exchangeAccount)
    assert.ok(!exchangeAccountAfter.debtShares.eq(new BN(0)))
    assert.ok(await (await usdToken.getAccountInfo(usdTokenAccount)).amount.eq(mintAmount))
  })
  it('withdraw', async () => {
    const accountOwner = new Account()
    const exchangeAccount = await exchange.createExchangeAccount(accountOwner.publicKey)
    const amount = new BN(10 * 1e6)
    const listOffset = 2

    const tokenAccounts = await Promise.all(
      tokens.map(async (collateralToken) => collateralToken.createAccount(accountOwner.publicKey))
    )

    await waitForBeggingOfASlot(connection)
    // Deposit tokens
    await Promise.all(
      tokens.slice(listOffset, 10).map(async (collateralToken, index) => {
        const tokenIndeks = index + listOffset
        const userCollateralTokenAccount = tokenAccounts[tokenIndeks]

        await collateralToken.mintTo(userCollateralTokenAccount, wallet, [], tou64(amount))

        await exchange.deposit({
          amount,
          exchangeAccount,
          owner: accountOwner.publicKey,
          userCollateralAccount: userCollateralTokenAccount,
          reserveAccount: reserves[tokenIndeks],
          collateralToken: tokens[tokenIndeks],
          exchangeAuthority,
          signers: [wallet, accountOwner]
        })
      })
    )

    await waitForBeggingOfASlot(connection)
    // Withdraw tokens
    await Promise.all(
      tokens.slice(listOffset, 10).map(async (collateralToken, index) => {
        const tokenIndeks = index + listOffset
        const userCollateralAccount = tokenAccounts[tokenIndeks]

        await exchange.withdraw({
          amount,
          reserveAccount: reserves[tokenIndeks],
          exchangeAccount,
          owner: accountOwner.publicKey,
          userCollateralAccount,
          signers: [accountOwner]
        })
      })
    )

    // Check saldos
    const tokenAccountsDataAfter = await Promise.all(
      tokenAccounts
        .slice(listOffset, 10)
        .map((account, index) => tokens[index + listOffset].getAccountInfo(account))
    )
    const exchangeAccountDataAfter = await exchange.getExchangeAccount(exchangeAccount)

    assert.ok(tokenAccountsDataAfter.every((i) => i.amount.eq(amount)))
    assert.ok(exchangeAccountDataAfter.collaterals.every((i) => i.amount.eq(new BN(0))))
  })
  it.only('swap', async () => {
    const accountOwner = new Account()
    const exchangeAccount = await exchange.createExchangeAccount(accountOwner.publicKey)
    const collateralAmount = new BN(100)

    const reserveAddress = reserves[2]
    const collateralToken = tokens[2]

    const userCollateralTokenAccount = await collateralToken.createAccount(accountOwner.publicKey)

    await collateralToken.mintTo(userCollateralTokenAccount, wallet, [], tou64(collateralAmount))

    await exchange.deposit({
      amount: collateralAmount,
      exchangeAccount,
      owner: accountOwner.publicKey,
      userCollateralAccount: userCollateralTokenAccount,
      reserveAccount: reserveAddress,
      collateralToken: collateralToken,
      exchangeAuthority,
      signers: [wallet, accountOwner]
    })

    const mintAmount = mulByPercentage(collateralAmount, healthFactor)
    const btcTokenAccount = await btcSynthetic.createAccount(accountOwner.publicKey)
    const usdTokenAccount = await usdToken.createAccount(accountOwner.publicKey)
    await exchange.mint({
      amount: mintAmount,
      exchangeAccount,
      owner: accountOwner.publicKey,
      to: usdTokenAccount,
      signers: [accountOwner]
    })

    await exchange.swap({
      amount: new BN(1),
      exchangeAccount,
      owner: accountOwner.publicKey,
      userTokenAccountFor: btcTokenAccount,
      userTokenAccountIn: usdTokenAccount,
      tokenFor: btcSynthetic.publicKey,
      tokenIn: usdToken.publicKey,
      signers: [accountOwner]
    })
  })
})
